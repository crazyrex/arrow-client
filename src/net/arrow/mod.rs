// Copyright 2015 click2stream, Inc.
// 
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
// 
//     http://www.apache.org/licenses/LICENSE-2.0
// 
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Arrow Protocol implementation.

pub mod error;
pub mod protocol;

use std::io;
use std::cmp;
use std::mem;
use std::result;

use std::ffi::CStr;
use std::error::Error;
use std::collections::VecDeque;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::io::{Read, Write, ErrorKind};

use utils;

use net::raw::ether::MacAddr;
use net::utils::{Timeout, WriteBuffer};

use utils::logger::Logger;
use utils::config::AppContext;
use utils::{Shared, Serialize};

use self::protocol::*;
use self::error::{Result, ArrowError};

use mio::tcp::TcpStream;
use mio::{EventLoop, EventSet, Token, PollOpt, Handler};

use openssl::ssl::{NonblockingSslStream, IntoSsl};
use openssl::ssl::error::NonblockingSslError;

/// Register a given TCP stream in a given event loop.
fn register_socket<H: Handler>(
    token_id: usize, 
    stream: &TcpStream, 
    readable: bool,
    writable: bool, 
    event_loop: &mut EventLoop<H>) {
    let poll       = PollOpt::level();
    let mut events = EventSet::all();
    
    if !readable {
        events.remove(EventSet::readable());
    }
    
    if !writable {
        events.remove(EventSet::writable());
    }
    
    event_loop.register(stream, Token(token_id), events, poll)
        .unwrap();
}

/// Re-register a given TCP stream in a given event loop.
fn reregister_socket<H: Handler>(
    token_id: usize, 
    stream: &TcpStream, 
    readable: bool,
    writable: bool, 
    event_loop: &mut EventLoop<H>) {
    let poll       = PollOpt::level();
    let mut events = EventSet::all();
    
    if !readable {
        events.remove(EventSet::readable());
    }
    
    if !writable {
        events.remove(EventSet::writable());
    }
    
    event_loop.reregister(stream, Token(token_id), events, poll)
        .unwrap();
}

/// Deregister a given socket.
fn deregister_socket<H: Handler>(
    stream: &TcpStream, 
    event_loop: &mut EventLoop<H>) {
    event_loop.deregister(stream)
        .unwrap();
}

/// Commands that might be sent by the Arrow Client into a given mpsc queue.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Command {
    ResetServiceTable,
    ScanNetwork,
}

/// Common trait for various implementations of command senders.
pub trait Sender<C: Send> {
    /// Send a given command or return the command back if the send operation 
    /// failed.
    fn send(&self, cmd: C) -> result::Result<(), C>;
}

/// ArrowStream states.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum ArrowStreamState {
    Ok,
    ReaderWantRead,
    ReaderWantWrite,
    WriterWantRead,
    WriterWantWrite,
}

/// Abstraction over the Arrow SSL stream.
struct ArrowStream {
    stream:   NonblockingSslStream<TcpStream>,
    state:    ArrowStreamState,
    token_id: usize,
}

impl ArrowStream {
    /// Create a new ArrowStream instance and register the underlaying socket 
    /// within a given event loop.
    fn connect<S: IntoSsl, H: Handler>(
        s: S, 
        arrow_addr: &SocketAddr,
        token_id: usize,
        event_loop: &mut EventLoop<H>) -> Result<ArrowStream> {
        let tcp_stream = try!(TcpStream::connect(arrow_addr));
        let ssl_stream = try!(NonblockingSslStream::connect(s, tcp_stream));
        
        register_socket(token_id, ssl_stream.get_ref(), 
            true, true, event_loop);
        
        let res = ArrowStream {
            stream:   ssl_stream,
            state:    ArrowStreamState::Ok,
            token_id: token_id
        };
        
        Ok(res)
    }
    
    /// Enable receiving writable events for the underlaying TCP socket.
    fn enable_socket_events<H: Handler>(
        &mut self, 
        readable: bool, 
        writable: bool, 
        event_loop: &mut EventLoop<H>) {
        reregister_socket(self.token_id, self.stream.get_ref(), 
            readable, writable, event_loop);
    }
    
    /// Read available data from the underlaying SSL stream into a given 
    /// buffer.
    fn read<H: Handler>(
        &mut self, 
        buf: &mut [u8], 
        event_loop: &mut EventLoop<H>) -> Result<usize> {
        match self.stream.read(buf) {
            Err(NonblockingSslError::WantRead) => {
                self.state = ArrowStreamState::ReaderWantRead;
                self.enable_socket_events(true, false, event_loop);
                Ok(0)
            },
            Err(NonblockingSslError::WantWrite) => {
                self.state = ArrowStreamState::ReaderWantWrite;
                self.enable_socket_events(false, true, event_loop);
                Ok(0)
            },
            other => {
                self.state = ArrowStreamState::Ok;
                self.enable_socket_events(true, true, event_loop);
                Ok(try!(other))
            }
        }
    }
    
    /// Write given data using the underlaying SSL stream.
    fn write<H: Handler>(
        &mut self, 
        data: &[u8], 
        event_loop: &mut EventLoop<H>) -> Result<usize> {
        match self.stream.write(data) {
            Err(NonblockingSslError::WantRead) => {
                self.state = ArrowStreamState::WriterWantRead;
                self.enable_socket_events(true, false, event_loop);
                Ok(0)
            },
            Err(NonblockingSslError::WantWrite) => {
                self.state = ArrowStreamState::WriterWantWrite;
                self.enable_socket_events(false, true, event_loop);
                Ok(0)
            },
            other => {
                self.state = ArrowStreamState::Ok;
                self.enable_socket_events(true, true, event_loop);
                Ok(try!(other))
            }
        }
    }
    
    /// Check if the underlaying socket is ready to read.
    fn can_read(&self, event_set: EventSet) -> bool {
        match self.state {
            ArrowStreamState::Ok              => event_set.is_readable(),
            ArrowStreamState::ReaderWantRead  => event_set.is_readable(),
            ArrowStreamState::ReaderWantWrite => event_set.is_writable(),
            _ => false
        }
    }
    
    /// Check if the underlaying socket is ready to write.
    fn can_write(&self, event_set: EventSet) -> bool {
        match self.state {
            ArrowStreamState::Ok              => event_set.is_writable(),
            ArrowStreamState::WriterWantRead  => event_set.is_readable(),
            ArrowStreamState::WriterWantWrite => event_set.is_writable(),
            _ => false
        }
    }
    
    fn take_socket_error(&self) -> io::Result<()> {
        self.stream.get_ref()
            .take_socket_error()
    }
}

/// TCP stream abstraction for ignoring EWOULDBLOCKs.
struct ServiceStream {
    /// TCP stream.
    stream: TcpStream,
}

impl ServiceStream {
    /// Connect to a given TCP socket address.
    fn connect(addr: &SocketAddr) -> io::Result<ServiceStream> {
        let stream = try!(TcpStream::connect(addr));
        let res    = ServiceStream {
            stream: stream
        };
        
        Ok(res)
    }
    
    /// Get reference to the underlaying TCP stream.
    fn get_ref(&self) -> &TcpStream {
        &self.stream
    }
    
    /// Take error from the underlaying TCP stream.
    fn take_socket_error(&self) -> io::Result<()> {
        self.stream.take_socket_error()
    }
}

impl Read for ServiceStream {
    /// Read data from the underlaying socket (EWOULDBLOCK is silently 
    /// ignored).
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.stream.read(buf) {
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => Ok(0),
            other => other
        }
    }
}

impl Write for ServiceStream {
    /// Write data into the underlaying socket (EWOULDBLOCK is silently 
    /// ignored).
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.stream.write(buf) {
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => Ok(0),
            other => other
        }
    }
    
    /// Flush buffered data into the underlaying socket (EWOULDBLOCK is not
    /// ignored in this case).
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// External service session context.
/// 
/// This struct holds connection to an external service (e.g. RTSP) and 
/// its I/O buffers.
struct SessionContext<L: Logger> {
    /// Logger.
    #[allow(dead_code)]
    logger:        L,
    /// Service ID.
    service_id:    u16,
    /// Session ID.
    session_id:    u32,
    /// TCP stream.
    stream:        ServiceStream,
    /// Input buffer.
    input_buffer:  WriteBuffer,
    /// Output buffer.
    output_buffer: WriteBuffer,
    /// Read buffer.
    read_buffer:   Box<[u8]>,
    /// Write timeout.
    write_tout:    Timeout,
}

impl<L: Logger> SessionContext<L> {
    /// Create a new session context for a given session ID and service 
    /// address.
    fn new<T: Handler>(
        logger:     L,
        service_id: u16,
        session_id: u32, 
        addr: &SocketAddr,
        event_loop: &mut EventLoop<T>) -> Result<SessionContext<L>> {
        let stream = try!(ServiceStream::connect(addr));
        
        register_socket(session2token(session_id), stream.get_ref(), 
            true, true, event_loop);
        
        let res = SessionContext {
            logger:        logger,
            service_id:    service_id,
            session_id:    session_id,
            stream:        stream,
            input_buffer:  WriteBuffer::new(256 * 1024),
            output_buffer: WriteBuffer::new(0),
            read_buffer:   Box::new([0u8; 32768]),
            write_tout:    Timeout::new()
        };
        
        Ok(res)
    }
    
    /// Dispose resources held by this object.
    fn dispose<T: Handler>(&self, event_loop: &mut EventLoop<T>) {
        deregister_socket(self.stream.get_ref(), event_loop);
    }
    
    /// Enable/disable notifications for the underlaying socket.
    fn update_socket_events<T: Handler>(
        &mut self, 
        event_loop: &mut EventLoop<T>) {
        let readable = !self.input_buffer.is_full();
        let writable = !self.output_buffer.is_empty();
        reregister_socket(
            session2token(self.session_id), 
            self.stream.get_ref(), 
            readable, writable, event_loop);
    }
    
    /// Process a given set of socket events and return size of the input 
    /// buffer or None in case the connection has been closed.
    fn socket_ready<T: Handler>(
        &mut self, 
        event_loop: &mut EventLoop<T>, 
        event_set: EventSet) -> Result<Option<usize>> {
        try!(self.check_read_event(event_loop, event_set));
        try!(self.check_write_event(event_loop, event_set));
        
        if event_set.is_error() {
            let err = self.get_socket_error()
                .ok_or(ArrowError::from("socket error expected"));
            Err(try!(err))
        } else if event_set.is_hup() {
            Ok(None)
        } else {
            Ok(Some(self.input_buffer.buffered()))
        }
    }
    
    /// Read a message if the underlaying socket is readable and the input 
    /// buffer is not already full.
    fn check_read_event<T: Handler>(
        &mut self, 
        event_loop: &mut EventLoop<T>, 
        event_set: EventSet) -> Result<()> {
        if event_set.is_readable() {
            if self.input_buffer.is_full() {
                self.update_socket_events(event_loop);
            } else {
                let buffer = &mut *self.read_buffer;
                let len    = try!(self.stream.read(buffer));
                self.input_buffer.write_all(&buffer[..len])
                    .unwrap();
                
                //log_debug!(self.logger, &format!("{} bytes read from session socket {:08x} (buffer size: {})", len, self.session_id, self.input_buffer.buffered()));
            }
        }
        
        Ok(())
    }
    
    /// Write data from the output buffer into the underlaying socket if the 
    /// socket is writable.
    fn check_write_event<T: Handler>(
        &mut self, 
        event_loop: &mut EventLoop<T>, 
        event_set: EventSet) -> Result<()> {
        if event_set.is_writable() {
            if self.output_buffer.is_empty() {
                self.update_socket_events(event_loop);
                self.write_tout.clear();
            } else {
                let len = try!(self.stream.write(
                    self.output_buffer.as_bytes()));
                
                if len > 0 {
                    //log_debug!(self.logger, &format!("{} bytes written into session socket {:08x} (buffer size: {})", len, self.session_id, self.output_buffer.buffered()));
                    self.output_buffer.drop(len);
                    self.write_tout.set(CONNECTION_TIMEOUT);
                }
            }
        }
        
        Ok(())
    }
    
    /// Get socket error.
    fn get_socket_error(&self) -> Option<ArrowError> {
        let err = self.stream.take_socket_error();
        match err.err() {
            Some(err) => Some(ArrowError::from(err)),
            None      => None
        }
    }
    
    /// Check if there are some data in the input buffer.
    fn input_ready(&self) -> bool {
        !self.input_buffer.is_empty()
    }
    
    /// Get buffered input data.
    fn input_buffer(&self) -> &[u8] {
        self.input_buffer.as_bytes()
    }
    
    /// Drop a given number of bytes from the input buffer.
    fn drop_input_bytes<T: Handler>(
        &mut self, 
        count: usize, 
        event_loop: &mut EventLoop<T>) {
        let was_full = self.input_buffer.is_full();
        
        self.input_buffer.drop(count);
        
        if was_full && !self.input_buffer.is_full() {
            self.update_socket_events(event_loop);
        }
    }
    
    /// Send a given message.
    fn send_message<T: Handler>(
        &mut self, 
        data: &[u8], 
        event_loop: &mut EventLoop<T>) {
        let was_empty = self.output_buffer.is_empty();
        
        self.output_buffer.write_all(data)
            .unwrap();
        
        if was_empty {
            self.write_tout.set(CONNECTION_TIMEOUT);
            self.update_socket_events(event_loop);
        }
    }
}

/// Convert a given session ID into a token (socket) ID.
fn session2token(session_id: u32) -> usize {
    assert!(mem::size_of::<usize>() >= 4);
    (session_id as usize) | (1 << 24)
}

/// Convert a given token (socket) ID into a session ID.
fn token2session(token_id: usize) -> u32 {
    assert!(mem::size_of::<usize>() >= 4);
    let mask = ((1 as usize) << 24) - 1;
    assert!((token_id & !mask) == (1 << 24));
    (token_id & mask) as u32
}

/// Arrow Protocol states.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum ProtocolState {
    Handshake,
    Established
}

type SocketEventResult = Result<Option<String>>;

const UPDATE_CHECK_PERIOD:  u64 = 5000;
const TIMEOUT_CHECK_PERIOD: u64 = 1000;
const PING_PERIOD:          u64 = 60000;

const CONNECTION_TIMEOUT:   u64 = 20000;

/// Arrow client connection handler.
struct ConnectionHandler<L: Logger, Q: Sender<Command>> {
    /// Application logger.
    logger:        L,
    /// Shared application context.
    app_context:   Shared<AppContext>,
    /// Channel for sending Arrow Commands.
    cmd_sender:    Q,
    /// SSL/TLS connection to a remote Arrow Service.
    stream:        ArrowStream,
    /// Session contexts.
    sessions:      HashMap<u32, SessionContext<L>>,
    /// Session read queue.
    session_queue: VecDeque<u32>,
    /// Buffer for reading Arrow Protocol requests.
    read_buffer:   Box<[u8]>,
    /// Buffer for writing Arrow Protocol responses.
    write_buffer:  Box<[u8]>,
    /// Parser for requests received from Arrow Service.
    req_parser:    ArrowMessageParser,
    /// Output buffer for messages to be passed to Arrow Service.
    output_buffer: WriteBuffer,
    /// Arrow Client result returned after the connection shut down.
    result:        Option<Result<String>>,
    /// Protocol state.
    state:         ProtocolState,
    /// Version of the last sent service table.
    last_update:   Option<usize>,
    /// Write timeout.
    write_tout:    Timeout,
    /// ACK timeout.
    ack_tout:      Timeout,
    /// Current Control Message ID.
    msg_id:        u16,
    /// Expected ACKs.
    expected_acks: VecDeque<u16>,
}

impl<L: Logger + Clone, Q: Sender<Command>> ConnectionHandler<L, Q> {
    /// Create a new connection handler.
    fn new<S: IntoSsl>(
        logger: L,
        s: S, 
        cmd_sender: Q,
        addr: &SocketAddr, 
        arrow_mac: &MacAddr,
        app_context: Shared<AppContext>, 
        event_loop: &mut EventLoop<Self>) -> Result<Self> {
        let stream = try!(ArrowStream::connect(s, addr, 0, event_loop));
        
        let mut res = ConnectionHandler {
            logger:        logger,
            app_context:   app_context,
            cmd_sender:    cmd_sender,
            stream:        stream,
            sessions:      HashMap::new(),
            session_queue: VecDeque::new(),
            read_buffer:   Box::new([0u8; 32768]),
            write_buffer:  Box::new([0u8; 16384]),
            req_parser:    ArrowMessageParser::new(),
            output_buffer: WriteBuffer::new(256 * 1024),
            result:        None,
            state:         ProtocolState::Handshake,
            last_update:   None,
            write_tout:    Timeout::new(),
            ack_tout:      Timeout::new(),
            msg_id:        0,
            expected_acks: VecDeque::new()
        };
        
        res.create_register_request(arrow_mac, event_loop);
        
        // start timeout checker:
        event_loop.timeout_ms(TimerEvent::TimeoutCheck(0), TIMEOUT_CHECK_PERIOD)
            .unwrap();
        
        Ok(res)
    }
    
    /// Get session context for a given session ID.
    fn get_session_context(
        &self, 
        session_id: u32) -> Option<&SessionContext<L>> {
        self.sessions.get(&session_id)
    }
    
    /// Get session context for a given session ID.
    fn get_session_context_mut(
        &mut self, 
        session_id: u32) -> Option<&mut SessionContext<L>> {
        self.sessions.get_mut(&session_id)
    }
    
    /// Create a new session context for a given service and session IDs.
    fn create_session_context(
        &mut self, 
        service_id: u16, 
        session_id: u32, 
        event_loop: &mut EventLoop<Self>) -> Option<&mut SessionContext<L>> {
        if !self.sessions.contains_key(&session_id) {
            let app_context = self.app_context.lock()
                .unwrap();
            let config = &app_context.config;
            if let Some(svc) = config.get(service_id) {
                if let Some(addr) = svc.address() {
                    log_info!(self.logger, &format!("connecting to remote service: {}, session ID: {:08x}", addr, session_id));
                    match SessionContext::new(self.logger.clone(),
                        service_id, session_id, addr, event_loop) {
                        Err(err) => log_warn!(self.logger, &format!("unable to open connection to a remote service: {}", err.description())),
                        Ok(ctx)  => {
                            let token_id = session2token(session_id);
                            let tevent   = TimerEvent::TimeoutCheck(token_id);
                            self.sessions.insert(session_id, ctx);
                            self.session_queue.push_back(session_id);
                            event_loop.timeout_ms(tevent, TIMEOUT_CHECK_PERIOD)
                                .unwrap();
                        }
                    }
                } else {
                    log_warn!(self.logger, "requested service ID belongs to a Control Protocol service");
                }
            } else {
                log_warn!(self.logger, &format!("non-existing service requested (service ID: {})", service_id));
            }
        }
        
        self.sessions.get_mut(&session_id)
    }
    
    /// Remove session context with a given session ID.
    fn remove_session_context(
        &mut self, 
        session_id: u32,
        event_loop: &mut EventLoop<Self>) {
        if let Some(ctx) = self.sessions.remove(&session_id) {
            ctx.dispose(event_loop);
        }
    }
    
    /// Create a new REGISTER request.
    fn create_register_request(
        &mut self, 
        arrow_mac: &MacAddr, 
        event_loop: &mut EventLoop<Self>) {
        let control_msg = {
            let app_context = self.app_context.lock()
                .unwrap();
            let config = &app_context.config;
            let msg    = RegisterMessage::new(
                config.uuid(),
                arrow_mac.octets(),
                config.password(),
                config.service_table());
            let control_msg = control::create_register_message(self.msg_id, 
                msg);
            self.last_update = Some(config.version());
            self.msg_id += 1;
            control_msg
        };
        
        log_debug!(self.logger, "sending REGISTER request...");
        
        self.send_unconfirmed_control_message(control_msg, event_loop);
    }
    
    /// Send an update message (if needed) and schedule the next update event.
    fn send_update_message(
        &mut self,
        svc_table: ServiceTable,
        event_loop: &mut EventLoop<Self>) {
        let control_msg = control::create_update_message(self.msg_id, 
            svc_table);
            
        self.msg_id += 1;
        
        log_debug!(self.logger, "sending an UPDATE message...");
        
        self.send_control_message(control_msg, event_loop);
    }
    
    /// Send the PING message and schedule the next PING event.
    fn send_ping_message(&mut self, event_loop: &mut EventLoop<Self>) {
        let control_msg = control::create_ping_message(self.msg_id);
        
        self.msg_id += 1;
        
        log_debug!(self.logger, "sending a PING message...");
        
        self.send_unconfirmed_control_message(control_msg, event_loop);
    }
    
    /// Send HUP message for a given session ID.
    fn send_hup_message(
        &mut self, 
        session_id: u32, 
        error_code: u32, 
        event_loop: &mut EventLoop<Self>) {
        let control_msg = control::create_hup_message(self.msg_id, 
            session_id, error_code);
        
        self.msg_id += 1;
        
        log_debug!(self.logger, "sending a HUP message...");
        
        self.send_control_message(control_msg, event_loop);
    }
    
    /// Send status message for a given request ID.
    fn send_status(
        &mut self,
        request_id: u16,
        event_loop: &mut EventLoop<Self>) {
        let active_sessions  = self.sessions.len() as u32;
        let mut status_flags = 0;
        
        {
            let app_context = self.app_context.lock()
                .unwrap();
            
            if app_context.scanning {
                status_flags |= control::STATUS_FLAG_SCAN;
            }
        }
        
        let status_msg = StatusMessage::new(request_id, 
            status_flags, active_sessions);
        let control_msg = control::create_status_message(self.msg_id,
            status_msg);
        
        self.msg_id += 1;
        
        log_debug!(self.logger, "sending a STATUS message...");
        
        self.send_control_message(control_msg, event_loop);
    }
    
    /// Send ACK message with a given message id and error code.
    fn send_ack_message(
        &mut self,
        msg_id: u16,
        error_code: u32,
        event_loop: &mut EventLoop<Self>) {
        let control_msg = control::create_ack_message(msg_id, error_code);
        
        log_debug!(self.logger, "sending and ACK message...");
        
        self.send_control_message(control_msg, event_loop);
    }
    
    /// Send a given Control protocol message.
    fn send_control_message<B: ControlMessageBody>(
        &mut self,
        control_msg: ControlMessage<B>,
        event_loop: &mut EventLoop<Self>) {
        let arrow_msg = ArrowMessage::new(0, 0, control_msg);
        self.send_message(&arrow_msg, event_loop);
    }
    
    /// Send a given Control Protocol message which needs to be confirmed by 
    // ACK.
    fn send_unconfirmed_control_message<B: ControlMessageBody>(
        &mut self, 
        control_msg: ControlMessage<B>, 
        event_loop: &mut EventLoop<Self>) {
        if self.expected_acks.is_empty() {
            self.ack_tout.set(CONNECTION_TIMEOUT);
        }
        
        let msg_id = control_msg.header()
            .msg_id;
        
        self.expected_acks.push_back(msg_id);
        
        self.send_control_message(control_msg, event_loop);
    }
    
    /// Send a given Arrow Message.
    fn send_message<B: ArrowMessageBody>(
        &mut self, 
        arrow_msg: &ArrowMessage<B>, 
        event_loop: &mut EventLoop<Self>) {
        if self.output_buffer.is_empty() {
            self.write_tout.set(CONNECTION_TIMEOUT);
        }
        
        arrow_msg.serialize(&mut self.output_buffer)
            .unwrap();
        
        self.stream.enable_socket_events(true, true, event_loop);
    }
    
    /// Check if the service table has been updated and send an UPDATE message
    /// if needed.
    fn check_update(&mut self, event_loop: &mut EventLoop<Self>) {
        let cur_version;
        let svc_table;
        
        {
            let app_context = self.app_context.lock()
                .unwrap();
            let config  = &app_context.config;
            cur_version = config.version();
            svc_table   = config.service_table();
        }
        
        let send_update = match self.last_update {
            Some(sent_version) => cur_version > sent_version,
            None => true
        };
        
        if send_update {
            self.send_update_message(svc_table, event_loop);
            self.last_update = Some(cur_version);
        }
    }
    
    /// Check if the service table has been updated and send an UPDATE message
    /// if needed.
    fn te_check_update(
        &mut self, 
        event_loop: &mut EventLoop<Self>) -> Result<()> {
        self.check_update(event_loop);
        
        event_loop.timeout_ms(TimerEvent::Update, UPDATE_CHECK_PERIOD)
            .unwrap();
        
        Ok(())
    }
    
    /// Periodical connection check.
    fn te_check_connection(
        &mut self, 
        event_loop: &mut EventLoop<Self>) -> Result<()> {
        self.send_ping_message(event_loop);
        
        event_loop.timeout_ms(TimerEvent::Ping, PING_PERIOD)
            .unwrap();
        
        Ok(())
    }
    
    /// Check connection timeout.
    fn te_check_timeout(
        &mut self,
        token: usize, 
        event_loop: &mut EventLoop<Self>) -> Result<()> {
        match token {
            0 => self.check_arrow_timeout(event_loop),
            t => self.check_session_timeout(token2session(t), event_loop)
        }
    }
    
    /// Check connection timeout of the underlaying Arrow socket.
    fn check_arrow_timeout(
        &mut self, 
        event_loop: &mut EventLoop<Self>) -> Result<()> {
        if !self.write_tout.check() || !self.ack_tout.check() {
            Err(ArrowError::from("Arrow Service connection timeout"))
        } else {
            event_loop.timeout_ms(TimerEvent::TimeoutCheck(0), 
                TIMEOUT_CHECK_PERIOD).unwrap();
            
            Ok(())
        }
    }
    
    /// Check session communication timeout.
    fn check_session_timeout(
        &mut self, 
        session_id: u32, 
        event_loop: &mut EventLoop<Self>) -> Result<()> {
        let mut timeout = false;
        
        if let Some(ctx) = self.get_session_context(session_id) {
            timeout = !ctx.write_tout.check();
        }
        
        if timeout {
            log_warn!(self.logger, &format!("session {} connection timeout", session_id));
            self.send_hup_message(session_id, 0, event_loop);
            self.remove_session_context(session_id, event_loop);
        } else {
            event_loop.timeout_ms(
                TimerEvent::TimeoutCheck(session2token(session_id)), 
                TIMEOUT_CHECK_PERIOD).unwrap();
        }
        
        Ok(())
    }
    
    /// Process all notifications for the underlaying TLS socket.
    fn arrow_socket_ready(
        &mut self, 
        event_loop: &mut EventLoop<Self>, 
        event_set: EventSet) -> SocketEventResult {
        let res = try!(self.check_arrow_read_event(event_loop, event_set));
        if res.is_some() {
            return Ok(res);
        }
        
        let res = try!(self.check_arrow_write_event(event_loop, event_set));
        if res.is_some() {
            return Ok(res);
        }
        
        if event_set.is_error() {
            let socket_err = self.stream.take_socket_error();
            Err(ArrowError::from(socket_err.unwrap_err()))
        } else if event_set.is_hup() {
            Err(ArrowError::from("connection to Arrow Service lost"))
        } else {
            Ok(None)
        }
    }
    
    /// Read a request/response chunk if the underlaying TLS socket is 
    /// readable.
    fn check_arrow_read_event(
        &mut self, 
        event_loop: &mut EventLoop<Self>, 
        event_set: EventSet) -> SocketEventResult {
        if self.stream.can_read(event_set) {
            self.read_request(event_loop)
        } else {
            Ok(None)
        }
    }
    
    /// Write a request/response chunk if the underlaying TLS socket is 
    /// writable.
    fn check_arrow_write_event(
        &mut self, 
        event_loop: &mut EventLoop<Self>, 
        event_set: EventSet) -> SocketEventResult {
        if self.stream.can_write(event_set) {
            self.send_response(event_loop)
        } else {
            Ok(None)
        }
    }
    
    /// Read request data from the underlaying TLS socket.
    fn read_request(
        &mut self, 
        event_loop: &mut EventLoop<Self>) -> SocketEventResult {
        let mut consumed = 0;
        
        let len = try!(self.stream.read(&mut *self.read_buffer, event_loop));
        
        //log_debug!(self.logger, &format!("{} bytes read from the Arrow socket", len));
        
        while consumed < len {
            consumed += try!(self.req_parser.add(
                &self.read_buffer[consumed..len]));
            if self.req_parser.is_complete() {
                let redirect = try!(self.process_request(event_loop));
                if redirect.is_some() {
                    return Ok(redirect);
                }
            }
        }
        
        Ok(None)
    }
    
    /// Parse the last complete request.
    /// 
    /// # Panics
    /// If the last request has not been completed yet.
    fn process_request(
        &mut self, 
        event_loop: &mut EventLoop<Self>) -> SocketEventResult {
        let service_id;
        let session_id;
        
        if let Some(header) = self.req_parser.header() {
            service_id = header.service;
            session_id = header.session;
        } else {
            panic!("incomplete message")
        }
        
        match service_id {
            0 => self.process_control_message(event_loop),
            _ => self.process_service_request(service_id, session_id, 
                event_loop)
        }
    }
    
    /// Process a Control Protocol message.
    fn process_control_message(
        &mut self, 
        event_loop: &mut EventLoop<Self>) -> SocketEventResult {
        let (header, body) = try!(self.parse_control_message());
        
        log_debug!(self.logger, &format!("received control message: {:?}", header.message_type()));
        
        let res = match header.message_type() {
            ControlMessageType::ACK => 
                self.process_ack_message(header.msg_id, &body, event_loop),
            ControlMessageType::PING =>
                self.process_ping_message(header.msg_id, event_loop),
            ControlMessageType::REDIRECT =>
                self.process_redirect_message(&body),
            ControlMessageType::HUP =>
                self.process_hup_message(&body, event_loop),
            ControlMessageType::RESET_SVC_TABLE =>
                self.process_command(Command::ResetServiceTable),
            ControlMessageType::SCAN_NETWORK =>
                self.process_command(Command::ScanNetwork),
            ControlMessageType::GET_STATUS =>
                self.process_status_request(header.msg_id, event_loop),
            mt => Err(ArrowError::from(format!("cannot handle Control Protocol message type: {:?}", mt)))
        };
        
        self.req_parser.clear();
        
        res
    }
    
    /// Parse a Control Protocol message from the underlaying Arrow Message 
    /// parser.
    fn parse_control_message(&self) -> Result<(ControlMessageHeader, Vec<u8>)> {
        if let Some(body) = self.req_parser.body() {
            let mut parser = ControlMessageParser::new();
            try!(parser.process(body));
            let header = parser.header();
            let body   = parser.body();
            if header.message_type() == ControlMessageType::UNKNOWN {
                Err(ArrowError::from("unknown Control Protocol message type"))
            } else {
                Ok((header.clone(), body.to_vec()))
            }
        } else {
            panic!("incomplete message");
        }
    }
    
    /// Process a Control Protocol ACK message.
    fn process_ack_message(
        &mut self, 
        msg_id: u16, 
        msg: &[u8],
        event_loop: &mut EventLoop<Self>) -> SocketEventResult {
        let expected_ack = self.expected_acks.pop_front();
        
        if self.expected_acks.is_empty() {
            self.ack_tout.clear();
        } else {
            self.ack_tout.set(CONNECTION_TIMEOUT);
        }
        
        if let Some(expected_ack) = expected_ack {
            if msg_id == expected_ack {
                if self.state == ProtocolState::Handshake {
                    self.process_handshake_ack(msg, event_loop)
                } else {
                    Ok(None)
                }
            } else {
                Err(ArrowError::from("unexpected ACK message ID"))
            }
        } else {
            Err(ArrowError::from("no ACK message expected"))
        }
    }
    
    /// Process ACK response for the REGISTER command.
    fn process_handshake_ack(
        &mut self, 
        msg: &[u8],
        event_loop: &mut EventLoop<Self>) -> SocketEventResult {
        if self.state == ProtocolState::Handshake {
            let ack = try!(control::parse_ack_message(msg));
            if ack == 0 {
                // switch the protocol state into normal operation
                self.state = ProtocolState::Established;
                // start sending update messages
                event_loop.timeout_ms(TimerEvent::Update, 
                    UPDATE_CHECK_PERIOD).unwrap();
                // start sending PING messages
                event_loop.timeout_ms(TimerEvent::Ping,
                    PING_PERIOD).unwrap();
                
                Ok(None)
            } else {
                Err(ArrowError::from("Arrow REGISTER failed"))
            }
        } else {
            panic!("unexpected protocol state");
        }
    }
    
    /// Process a Control Protocol PING message.
    fn process_ping_message(
        &mut self, 
        msg_id: u16, 
        event_loop: &mut EventLoop<Self>) -> SocketEventResult {
        if self.state == ProtocolState::Established {
            self.send_ack_message(msg_id, 0, event_loop);
            Ok(None)
        } else {
            Err(ArrowError::from("cannot handle PING message in the Handshake state"))
        }
    }
    
    /// Process a Control Protocol REDIRECT message.
    fn process_redirect_message(&mut self, msg: &[u8]) -> SocketEventResult {
        if self.state == ProtocolState::Established {
            let ptr  = msg.as_ptr();
            let cstr = unsafe {
                CStr::from_ptr(ptr as *const _)
            };
            
            let addr = String::from_utf8_lossy(cstr.to_bytes());
            
            Ok(Some(addr.to_string()))
        } else {
            Err(ArrowError::from("cannot handle REDIRECT message in the Handshake state"))
        }
    }
    
    /// Process a Control Protocol HUP message.
    fn process_hup_message(
        &mut self, 
        msg: &[u8], 
        event_loop: &mut EventLoop<Self>) -> SocketEventResult {
        if self.state == ProtocolState::Established {
            let msg        = try!(HupMessage::from_bytes(msg));
            let session_id = msg.session_id;
            // XXX: the HUP error code should be processed here
            log_info!(self.logger, &format!("session {:08x} closed", session_id));
            self.remove_session_context(session_id, event_loop);
            Ok(None)
        } else {
            Err(ArrowError::from("cannot handle HUP message in the Handshake state"))
        }
    }
    
    /// Send command using the underlaying command channel.
    fn process_command(&mut self, cmd: Command) -> SocketEventResult {
        match self.cmd_sender.send(cmd) {
            Err(cmd) => log_warn!(self.logger, &format!("unable to process command {:?}", cmd)),
            _ => ()
        }
        
        Ok(None)
    }
    
    /// Process status request (GET_STATUS message) with a given ID.
    fn process_status_request(
        &mut self, 
        msg_id: u16, 
        event_loop: &mut EventLoop<Self>) -> SocketEventResult {
        self.send_status(msg_id, event_loop);
        Ok(None)
    }
    
    /// Process request for a remote service.
    fn process_service_request(
        &mut self, 
        service_id: u16,
        session_id: u32,
        event_loop: &mut EventLoop<Self>) -> SocketEventResult {
        if self.state == ProtocolState::Established {
            let request = match self.req_parser.body() {
                Some(body) => body.to_vec(),
                None => panic!("incomplete message")
            };
            
            self.req_parser.clear();
            
            let send_hup = match self.create_session_context(
                service_id, session_id, event_loop) {
                None      => true,
                Some(ctx) => {
                    ctx.send_message(&request, event_loop);
                    false
                }
            };
            
            if send_hup {
                self.send_hup_message(session_id, 1, event_loop);
            }
            
            Ok(None)
        } else {
            Err(ArrowError::from("cannot handle service requests in the Handshake state"))
        }
    }
    
    /// Fill the Arrow Protocol output buffer with data from session input 
    /// buffers.
    fn fill_output_buffer(&mut self, event_loop: &mut EventLoop<Self>) {
        // using round robin alg. here in order to avoid session read 
        // starvation
        let mut queue_size = self.session_queue.len();
        while queue_size > 0 && !self.output_buffer.is_full() {
            if let Some(session_id) = self.session_queue.pop_front() {
                if let Some(ctx) = self.sessions.get_mut(&session_id) {
                    // avoid sending empty packets
                    let len = if ctx.input_ready() {
                        let data = ctx.input_buffer();
                        let len  = cmp::min(32768, data.len());
                        let arrow_msg = ArrowMessage::new(
                            ctx.service_id, ctx.session_id, 
                            &data[..len]);
                        
                        if self.output_buffer.is_empty() {
                            self.write_tout.set(CONNECTION_TIMEOUT);
                        }
                        
                        arrow_msg.serialize(&mut self.output_buffer)
                            .unwrap();
                        
                        len
                    } else {
                        0
                    };
                    
                    ctx.drop_input_bytes(len, event_loop);
                    
                    self.session_queue.push_back(session_id);
                    
                    //log_debug!(self.logger, &format!("{} bytes moved from session {:08x} input buffer into the Arrow output buffer", len, session_id));
                }
            }
            
            queue_size -= 1;
        }
    }
    
    /// Send response data using the underlaying TLS socket.
    fn send_response(
        &mut self, 
        event_loop: &mut EventLoop<Self>) -> SocketEventResult {
        self.fill_output_buffer(event_loop);
        
        if self.output_buffer.is_empty() {
            self.stream.enable_socket_events(true, false, event_loop);
            self.write_tout.clear();
        } else {
            let len = {
                let data   = self.output_buffer.as_bytes();
                let len    = cmp::min(data.len(), self.write_buffer.len());
                let buffer = &mut self.write_buffer[..len];
                utils::memcpy(buffer, &data[..len]);
                try!(self.stream.write(buffer, event_loop))
            };
            
            if len > 0 {
                //log_debug!(self.logger, &format!("{} bytes written into the Arrow socket", len));
                self.write_tout.set(CONNECTION_TIMEOUT);
                self.output_buffer.drop(len);
            }
        }
        
        Ok(None)
    }
    
    /// Process all notifications for a given remote session socket.
    fn session_socket_ready(
        &mut self, 
        session_id: u32, 
        event_loop: &mut EventLoop<Self>, 
        event_set: EventSet) -> SocketEventResult {
        let res = match self.get_session_context_mut(session_id) {
            Some(ctx) => ctx.socket_ready(event_loop, event_set),
            None      => Ok(Some(0))
        };
        
        match res {
            Err(err) => {
                log_warn!(self.logger, &format!("service connection error: {}", err.description()));
                self.send_hup_message(session_id, 2, event_loop);
                self.remove_session_context(session_id, event_loop);
            },
            Ok(None) => {
                log_info!(self.logger, "service connection closed");
                self.send_hup_message(session_id, 0, event_loop);
                self.remove_session_context(session_id, event_loop);
            },
            Ok(Some(size)) if size > 0 => {
                self.stream.enable_socket_events(true, true, event_loop);
            },
            _ => ()
        }
        
        Ok(None)
    }
}

/// Types of epoll() timer events.
#[derive(Debug, Copy, Clone)]
enum TimerEvent {
    Update,
    Ping,
    TimeoutCheck(usize),
}

impl<L, Q> Handler for ConnectionHandler<L, Q>
    where L: Logger + Clone,
          Q: Sender<Command> {
    type Timeout = TimerEvent;
    type Message = ();
    
    /// Event loop handler method.
    fn ready(
        &mut self, 
        event_loop: &mut EventLoop<Self>, 
        token: Token, 
        event_set: EventSet) {
        let res = match token {
            Token(0)  => self.arrow_socket_ready(event_loop, event_set),
            Token(id) => self.session_socket_ready(token2session(id), 
                event_loop, event_set)
        };
        
        match res {
            Ok(None)           => (),
            Ok(Some(redirect)) => self.result = Some(Ok(redirect)),
            Err(err)           => self.result = Some(Err(err))
        }
        
        if self.result.is_some() {
            event_loop.shutdown();
        }
    }
    
    /// Timer handler method.
    fn timeout(&mut self, event_loop: &mut EventLoop<Self>, token: TimerEvent) {
        let res = match token {
            TimerEvent::Update => self.te_check_update(event_loop),
            TimerEvent::Ping   => self.te_check_connection(event_loop),
            TimerEvent::TimeoutCheck(token) => 
                self.te_check_timeout(token, event_loop)
        };
        
        match res {
            Err(err) => self.result = Some(Err(err)),
            _        => ()
        }
        
        if self.result.is_some() {
            event_loop.shutdown();
        }
    }
}

/// Arrow client.
pub struct ArrowClient<L: Logger + Clone, Q: Sender<Command>> {
    connection: ConnectionHandler<L, Q>,
    event_loop: EventLoop<ConnectionHandler<L, Q>>,
}

impl<L: Logger + Clone, Q: Sender<Command>> ArrowClient<L, Q> {
    /// Create a new Arrow client.
    pub fn new<S: IntoSsl>(
        logger: L,
        s: S, 
        cmd_sender: Q,
        addr: &SocketAddr, 
        arrow_mac: &MacAddr,
        app_context: Shared<AppContext>) -> Result<Self> {
        let mut event_loop    = try!(EventLoop::new());
        let connection        = try!(ConnectionHandler::new(
            logger, s, cmd_sender, 
            addr, arrow_mac, app_context, 
            &mut event_loop));
        
        let res = ArrowClient {
            connection: connection,
            event_loop: event_loop
        };
        
        Ok(res)
    }
    
    /// Connect to the remote Arrow Service and start listening for incoming
    /// requests. Return error or redirect address in case the connection has 
    /// been shut down.
    pub fn event_loop(&mut self) -> Result<String> {
        try!(self.event_loop.run(&mut self.connection));
        match self.connection.result {
            Some(ref res) => res.clone(),
            _             => panic!("result expected")
        }
    }
}