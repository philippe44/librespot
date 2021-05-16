use super::{Open, Sink, SinkAsBytes};
use crate::audio::AudioPacket;
use crate::config::AudioFormat;
use std::thread;
use std::time::Duration;
use std::sync::mpsc::{Sender, SyncSender, Receiver, TryRecvError};
use std::sync::mpsc;
use std::net::TcpListener;
use std::net::{Shutdown, TcpStream};
use std::io::prelude::*;
use std::io::{self, Write};
use byteorder::{ByteOrder, BigEndian};
use std::slice;

pub struct HTTPSink {
    format: AudioFormat,
    streamer: Option<thread::JoinHandle<()>>,
    sink: Option<SyncSender<Vec<u8>>>, 
    ctrl: Option<Sender<bool>>, 
    buffer: Vec<u8>,
    flow: bool,
    host: String,
}

fn handle_connection(mut stream: TcpStream, sink: &Receiver<Vec<u8>>, format: AudioFormat) -> io::Result<()> {
    let mut buf = [0; 512];
    let mut bytes = 0; 
    
    // read request from the socket and write response back (stream audio)
    match stream.read(&mut buf) {
        Ok(_) => {
            let ct = match format {
                AudioFormat::Ogg => "audio/ogg",
                AudioFormat::S16 => "audio/L16;rate=44100;channels=2",
                AudioFormat::S24 => "audio/L24;rate=44100;channels=2",
                AudioFormat::S32 => "audio/L32;rate=44100;channels=2",
                _ => "audio/unk",
            };
            let response = format!("HTTP/1.0 200 OK\r\n\
                            Connection: Close\r\n\
                            Content-Type: {}\r\n\
                            \r\n", ct);
            info!("Request =>\n{}\nResponded =>\n{}", String::from_utf8_lossy(&buf[..]), response);                                                 
            stream.write(response.as_bytes())?;
            loop {
                match sink.recv() {
                    Ok(data) => { 
                        bytes += data.len();
                        match format {
                            AudioFormat::S16 => {
                                let mut data16 = unsafe { slice::from_raw_parts_mut(data.as_ptr() as *mut u16, data.len() / 2) };
                                BigEndian::from_slice_u16(&mut data16);
                                let data8 = unsafe { slice::from_raw_parts(data16.as_ptr() as *const u8, data.len()) };
                                stream.write(&data8)?;                      
                            },
                            AudioFormat::Ogg => { stream.write(&data)?; },
                            _ => (),        
                        };  
                    },
                    Err(_) => break,
                }               
            }
        },  
        Err(e) => return Err(e),
    };
    stream.shutdown(Shutdown::Both)?;   
    info!("streamed {} bytes", bytes);
    Ok(())
}

impl Open for HTTPSink {
    // once per connection
    fn open(options: Option<String>, format: AudioFormat) -> Self {
        let mut flow = false;
        let mut host = String::from("0.0.0.0:8329");
        
        info!("Using HTTP sink with options: {:?} and format: {:?}", options, format);

        // parse the options    
        if let Some(options) = options {
            let tokens = options.split(",");
            for item in tokens {
                let pair: Vec<&str> = item.split("=").collect();
                match pair.as_slice() {
                    ["host", str] => host = String::from(*str),
                    ["flow"] => flow = true,
                    o => info!("unknown option: {:?}", o),
                }   
            }   
        }
        
        // create actual sink
        HTTPSink {
            format,
            streamer: None,
            sink: None,
            ctrl: None,
            buffer: Vec::new(),
            flow,
            host,
        }
    }
}

impl Sink for HTTPSink {
    // on playback (every track)
    fn start(&mut self) -> io::Result<()> {
        //
        if self.streamer.is_some() {
            info!("streamer thread already running");
            return Ok(());
        }
        
        let (tx, sink) = mpsc::sync_channel(0);
        self.sink = Some(tx);
        let (tx, ctrl) = mpsc::channel();
        self.ctrl = Some(tx);
        let host = self.host.clone();
        let format = self.format;
        
        self.streamer = Some(thread::spawn(move || {
            let listener = TcpListener::bind(host).unwrap();
            listener.set_nonblocking(true).expect("Cannot set non-blocking");
            
            info!("streamer thread listening");
            
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => match handle_connection(s, &sink, format) {
                        Err(e) => warn!("connection ended {:?}", e),
                        Ok(_) => break,
                    },  
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if ctrl.try_recv() == Err(TryRecvError::Disconnected) {
                            break;
                        }
                        debug!("sleeping HTTP....");
                        thread::sleep(Duration::from_millis(500));
                        continue;
                    },
                    Err(e) => panic!("encountered IO error: {}", e),
                }   
            }
            
            info!("streamer thread exited");
        }));
        
        info!("sink started");      
        Ok(())
    }

    // every track
    fn stop(&mut self) -> io::Result<()> {
        info!("sink stopping");
        if !self.flow {
            if let Some(streamer) = self.streamer.take()  {
                self.sink.take();
                self.ctrl.take();
                streamer.join().expect("Can't stop streamer thread");
                info!("streamer stopped");              
            }   
        }
        Ok(())
    }

    sink_as_bytes!();
}

impl SinkAsBytes for HTTPSink {
    fn write_bytes(&mut self, data: &[u8]) -> io::Result<()> {
        self.buffer.append(&mut data.to_vec());
        if let Some(sink) = &self.sink {
            match sink.try_send(self.buffer.as_slice().to_vec()) {
                Ok(_) => { self.buffer.drain(..); },
                Err(_) => { 
                    debug!("sink sleeping");
                    thread::sleep(Duration::from_millis(100));
                },
            }
        }
        Ok(())
    }
}
