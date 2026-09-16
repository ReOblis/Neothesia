use std::{
    fs::File,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use midi_file::midly::{self, MidiMessage, live::LiveEvent, num::u4};
use winit::event_loop::EventLoopProxy;

use crate::NeothesiaEvent;

#[derive(Clone, Debug)]
pub struct BtBridgeHandle {
    stream: Arc<Mutex<Option<TcpStream>>>,
}

impl PartialEq for BtBridgeHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.stream, &other.stream)
    }
}

impl Eq for BtBridgeHandle {}

impl BtBridgeHandle {
    pub fn send_midi(&self, channel: u4, message: MidiMessage) {
        let mut buf = Vec::with_capacity(8);
        let event = LiveEvent::Midi { channel, message };
        if event.write(&mut buf).is_ok() {
            let mut lock = self.stream.lock().unwrap();
            if let Some(stream) = lock.as_mut() {
                if let Err(e) = stream.write_all(&buf) {
                    log::warn!("Failed to send MIDI to Bluetooth bridge: {e}");
                }
                let _ = stream.flush();
            }
        }
    }

    pub fn stop_all(&self) {
        let mut lock = self.stream.lock().unwrap();
        if let Some(stream) = lock.as_mut() {
            for ch in 0..16 {
                let mut buf = Vec::with_capacity(8);
                let event = LiveEvent::Midi {
                    channel: u4::new(ch),
                    message: MidiMessage::Controller {
                        controller: 123.into(),
                        value: 0.into(),
                    },
                };
                if event.write(&mut buf).is_ok() {
                    let _ = stream.write_all(&buf);
                }
            }
            let _ = stream.flush();
        }
    }
}

pub struct BtBridgeService {
    handle: BtBridgeHandle,
    _child: Option<Child>,
}

impl BtBridgeService {
    pub fn start(proxy: EventLoopProxy<NeothesiaEvent>) -> (Self, BtBridgeHandle) {
        let stream_holder = Arc::new(Mutex::new(None::<TcpStream>));
        let handle = BtBridgeHandle {
            stream: stream_holder.clone(),
        };

        let port = 48888;
        let listener = match TcpListener::bind(format!("127.0.0.1:{port}")) {
            Ok(l) => l,
            Err(e) => {
                log::error!("Cannot bind BT bridge TCP port {port}: {e}");
                return (
                    Self {
                        handle: handle.clone(),
                        _child: None,
                    },
                    handle,
                );
            }
        };

        let child = spawn_python_bridge(port);

        let stream_holder_clone = stream_holder.clone();
        thread::spawn(move || {
            log::info!("BT Bridge TCP Server listening on 127.0.0.1:{port}");
            for incoming in listener.incoming() {
                match incoming {
                    Ok(mut stream) => {
                        let _ = stream.set_nodelay(true);
                        log::info!("Bluetooth Bridge connected via TCP!");
                        if let Ok(clone_stream) = stream.try_clone() {
                            *stream_holder_clone.lock().unwrap() = Some(clone_stream);
                        }

                        let mut buf = [0u8; 128];
                        let mut midi_acc: Vec<u8> = Vec::new();

                        loop {
                            match stream.read(&mut buf) {
                                Ok(0) => {
                                    log::info!("BT Bridge client disconnected.");
                                    *stream_holder_clone.lock().unwrap() = None;
                                    break;
                                }
                                Ok(n) => {
                                    midi_acc.extend_from_slice(&buf[..n]);
                                    process_midi_stream(&mut midi_acc, &proxy);
                                }
                                Err(e) => {
                                    log::warn!("BT Bridge read error: {e}");
                                    *stream_holder_clone.lock().unwrap() = None;
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("BT Bridge accept error: {e}");
                        thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        });

        (
            Self {
                handle: handle.clone(),
                _child: child,
            },
            handle,
        )
    }

    pub fn handle(&self) -> BtBridgeHandle {
        self.handle.clone()
    }
}

fn process_midi_stream(acc: &mut Vec<u8>, proxy: &EventLoopProxy<NeothesiaEvent>) {
    while !acc.is_empty() {
        if acc[0] < 0x80 {
            acc.remove(0);
            continue;
        }

        let status = acc[0];
        let msg_len = match status & 0xF0 {
            0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => 3,
            0xC0 | 0xD0 => 2,
            0xF0 => 1,
            _ => 1,
        };

        if acc.len() < msg_len {
            break;
        }

        let raw_msg: Vec<u8> = acc.drain(..msg_len).collect();
        if let Ok(event) = LiveEvent::parse(&raw_msg) {
            if let LiveEvent::Midi { channel, message } = event {
                match message {
                    midly::MidiMessage::NoteOn { key, vel } if vel == 0 => {
                        proxy
                            .send_event(NeothesiaEvent::MidiInput {
                                channel: channel.as_int(),
                                message: MidiMessage::NoteOff { key, vel },
                            })
                            .ok();
                    }
                    message => {
                        proxy
                            .send_event(NeothesiaEvent::MidiInput {
                                channel: channel.as_int(),
                                message,
                            })
                            .ok();
                    }
                }
            }
        }
    }
}

fn find_python() -> Option<PathBuf> {
    let candidates = [
        r"C:\Users\King\AppData\Local\Programs\Python\Python312\python.exe",
        r"C:\Users\King\AppData\Local\Programs\Python\Python314\python.exe",
        r"C:\Program Files\Python312\python.exe",
        r"python.exe",
    ];

    for c in &candidates {
        let path = PathBuf::from(c);
        if path.exists() {
            return Some(path);
        }
    }
    Some(PathBuf::from("python"))
}

fn spawn_python_bridge(port: u16) -> Option<Child> {
    let python = find_python()?;
    let bridge_script = std::env::current_dir()
        .ok()?
        .join("bt_bridge")
        .join("bridge.py");

    let bridge_path = if bridge_script.exists() {
        bridge_script
    } else {
        PathBuf::from(r"D:\JFF\Neothesia\bt_bridge\bridge.py")
    };

    if !bridge_path.exists() {
        log::warn!("Bridge script not found at {:?}", bridge_path);
        return None;
    }

    let log_path = bridge_path.parent().unwrap().join("bridge.log");
    let log_out = File::create(&log_path).ok();
    let log_err = log_out.as_ref().and_then(|f| f.try_clone().ok());

    log::info!("Spawning background BT Bridge using {:?}...", python);
    let mut cmd = Command::new(python);
    cmd.arg(&bridge_path).arg("--port").arg(port.to_string());

    if let (Some(out), Some(err)) = (log_out, log_err) {
        cmd.stdout(Stdio::from(out)).stderr(Stdio::from(err));
    }

    cmd.spawn()
        .map_err(|e| log::error!("Failed to spawn Python bridge: {e}"))
        .ok()
}
