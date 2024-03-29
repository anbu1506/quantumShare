use std::sync::{mpsc, Arc, Mutex};

use tauri::{api::dialog, Window};
use tokio::{
    io::{copy, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use crate::{
    utils::{create_or_incnum, padding, remove_padding},
    AcceptPayload, ReceivePayload, ReceivedPayload, SendPayload, SentPayload,
};

pub struct Sender<'a> {
    name: String,
    my_streams_addr: Vec<String>,
    receiver_ip: &'a str,
    receiver_port: &'a str,
    files: Vec<String>,
}

impl<'a> Sender<'a> {
    pub fn new() -> Sender<'a> {
        let name = hostname::get().unwrap();
        let name = name.to_str().unwrap().to_string();
        Sender {
            name,
            my_streams_addr: vec![],
            files: vec![],
            receiver_ip: "",
            receiver_port: "",
        }
    }

    fn add_file(&mut self, file_name: String) {
        self.files.push(file_name.to_owned());
    }

    pub async fn select_files(&mut self) {
        let future = async {
            let (tx, rx) = mpsc::channel::<String>();
            dialog::FileDialogBuilder::new().pick_files(move |path_bufs| {
                for path_buf in path_bufs.unwrap_or_else(|| vec![]) {
                    let path = path_buf.to_str().unwrap().to_owned();
                    tx.send(path).unwrap();
                }
            });

            rx.iter().for_each(|path| {
                self.add_file(path);
            });
        };
        future.await;
    }

    pub fn set_receiver_addr(&mut self, receiver_ip: &'a str, receiver_port: &'a str) {
        self.receiver_ip = receiver_ip;
        self.receiver_port = receiver_port;
    }

    async fn connect_nth_stream(
        &mut self,
        n: i32,
    ) -> Result<TcpStream, Box<dyn std::error::Error>> {
        println!("stream {} connecting to receiver...", n);
        let stream =
            tokio::net::TcpStream::connect(self.receiver_ip.to_owned() + ":" + &self.receiver_port)
                .await?;
        self.my_streams_addr.push(stream.peer_addr()?.to_string());
        println!("stream {} connected to receiver", n);
        Ok(stream)
    }

    async fn handle_transfer(
        file_path: &str,
        stream: &mut tokio::net::TcpStream,
        sender_name: &str,
        receiver_ip: String,
        window: Window,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let file_name = std::path::Path::new(file_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        let file_name = padding(file_name.to_string());
        //chech the file exists or not
        let file = tokio::fs::File::open(file_path).await?;
        //then sending file_name
        stream.write_all(file_name.as_bytes()).await?;
        //sending sender_name
        stream
            .write_all(padding(sender_name.to_string()).as_bytes())
            .await?;
        //then sending data
        let allowed = stream.read_i32().await?;
        if allowed == 0 {
            return Ok(());
        }
        let file_name0 = file_name.clone();
        let receiver_ip0 = receiver_ip.clone();
        window
            .emit(
                "onSend",
                SendPayload {
                    file_name: file_name0,
                    receiver_ip: receiver_ip0,
                },
            )
            .unwrap();
        let mut file_reader = tokio::io::BufReader::new(file);
        let bytes_transferred = copy(&mut file_reader, stream).await?;
        window
            .emit(
                "onSent",
                SentPayload {
                    file_name,
                    receiver_ip,
                    bytes_sent: bytes_transferred,
                },
            )
            .unwrap();
        println!("Transferred {} bytes.", bytes_transferred);
        Ok(())
    }

    pub async fn send(&mut self, window: Window) -> Result<(), Box<dyn std::error::Error>> {
        let mut handles = vec![];
        let mut i = 1;
        while self.files.len() != 0 {
            let mut stream = self.connect_nth_stream(i as i32).await?;
            let file_path = self.files.pop().unwrap().to_string();
            let sender_name = self.name.to_string();
            let windows = window.clone();
            let receiver_ip = self.receiver_ip.to_owned();
            stream.write_i32(1).await?; //indicating that a 'file' is coming
            let handle = tokio::spawn(async move {
                Self::handle_transfer(
                    file_path.as_str(),
                    &mut stream,
                    sender_name.as_str(),
                    receiver_ip,
                    windows,
                )
                .await
                .unwrap();
            });
            handles.push(handle);
            i += 1;
        }

        for handle in handles {
            handle.await?;
        }
        Ok(())
    }
}

pub struct Receiver<'a> {
    my_ip: &'a str,
    my_port: String,
}

impl<'a> Receiver<'a> {
    pub fn new() -> Receiver<'a> {
        Receiver {
            my_ip: "0.0.0.0",
            my_port: "8080".to_owned(),
        }
    }

    pub async fn listen_on(
        &mut self,
        port: String,
        window: Window,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.my_port = port;
        let listener =
            TcpListener::bind(self.my_ip.to_owned() + ":" + self.my_port.as_str()).await?;
        println!("Listening on port {}", self.my_port);
        let mut handles = vec![];
        let mut i = 0;
        loop {
            let windows = window.clone();
            let (mut stream, _) = listener.accept().await?;
            println!("connection accepted from sender {}", stream.peer_addr()?);

            let types = stream.read_i32().await?;

            if types == 1 {
                let mut file_name = [0u8; 255];
                stream.read_exact(&mut file_name).await?;
                let file_name = remove_padding(String::from_utf8(file_name.to_vec())?);

                let mut sender_name = [0u8; 255];
                stream.read_exact(&mut sender_name).await?;

                let sender_name = remove_padding(String::from_utf8(sender_name.to_vec())?);
                let allowed =
                    Self::authenticate(windows.clone(), i, file_name.clone(), sender_name.clone())
                        .await;
                if allowed == 1 {
                    println!("receiving file");
                    stream.write_i32(1).await?;
                    let handle = tokio::spawn(async move {
                        Self::receive_file(&mut stream, windows, file_name, sender_name)
                            .await
                            .unwrap()
                    });

                    handles.push(handle);
                } else if allowed == 0 {
                    stream.write_i32(0).await?;
                }
            } else if types == 2 {
                let allowed = Self::authenticate(
                    windows.clone(),
                    i,
                    "clip txt".to_string(),
                    stream.peer_addr().unwrap().to_string(),
                )
                .await;
                if allowed == 1 {
                    stream.write_i32(1).await?;
                    Self::receive_txt(&mut stream, windows).await?;
                    println!("receied text");
                } else if allowed == 0 {
                    stream.write_i32(0).await?;
                }
            }

            i += 1;
        }
    }

    async fn receive_file(
        stream: &mut TcpStream,
        window: Window,
        file_name: String,
        sender_name: String,
    ) -> Result<String, Box<dyn std::error::Error>> {
        window
            .emit(
                "onReceive",
                ReceivePayload {
                    file_name: file_name.to_string(),
                    sender_name: sender_name.to_string(),
                },
            )
            .unwrap();
        println!("receiving {} from {}", file_name, sender_name);

        let download_path = home::home_dir()
            .unwrap()
            .join("Downloads")
            .join(file_name.as_str());
        let mut dest_file = create_or_incnum(download_path).await?;
        let bytes_transferred = copy(stream, &mut dest_file).await?;
        println!(
            "Received {} bytes from {} .",
            bytes_transferred, sender_name
        );
        let received_payload = ReceivedPayload {
            file_name: file_name.clone(),
            bytes_received: bytes_transferred,
            sender_name: sender_name.to_string(),
        };
        println!("received payload {:?}", received_payload);
        window.emit("onReceived", received_payload).unwrap();
        Ok(file_name)
    }

    async fn receive_txt(
        stream: &mut TcpStream,
        window: Window,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut data = String::new();
        stream.read_to_string(&mut data).await?;
        window.emit("onTextReceive", data).unwrap();
        Ok(())
    }

    async fn authenticate(window: Window, i: i32, file_name: String, sender_name: String) -> i32 {
        let reacted = Arc::new(Mutex::new(false));
        let choice = Arc::new(Mutex::new(0));
        // window.emit("auth", "").unwrap();
        let event = format!("auth{}", i);
        let reacted0 = Arc::clone(&reacted);
        let choice0 = Arc::clone(&choice);
        window.once(event.clone(), move |event| {
            let mut a = reacted0.lock().unwrap();
            let mut b = choice0.lock().unwrap();
            *a = true;
            *b = event.payload().unwrap().parse().unwrap();
        });

        window
            .emit(
                "auth",
                AcceptPayload {
                    event,
                    file_name,
                    sender_name,
                },
            )
            .unwrap();
        println!("waiting for reaction... {}", i);
        while !reacted.lock().unwrap().clone() {}
        let result = *choice.lock().unwrap();
        result
    }
}
