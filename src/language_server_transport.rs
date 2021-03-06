use crossbeam_channel::{bounded, Receiver, Sender};
use fnv::FnvHashMap;
use jsonrpc_core::{self, Call, Output, Params, Version};
use lsp_types::notification::Notification;
use lsp_types::*;
use serde_json;
use std::io::{self, BufRead, BufReader, BufWriter, Error, ErrorKind, Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use types::*;

pub struct LanguageServerTransport {
    pub sender: Sender<ServerMessage>,
    pub receiver: Receiver<ServerMessage>,
    pub thread: thread::JoinHandle<()>,
}

pub fn start(cmd: &str, args: &[String]) -> LanguageServerTransport {
    info!("Starting Language server `{} {}`", cmd, args.join(" "));
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to start language server");

    let writer = BufWriter::new(child.stdin.take().expect("Failed to open stdin"));
    let reader = BufReader::new(child.stdout.take().expect("Failed to open stdout"));

    // XXX temporary way of tracing language server errors
    let mut stderr = BufReader::new(child.stderr.take().expect("Failed to open stderr"));
    let error_thread = thread::spawn(move || loop {
        let mut buf = String::new();
        match stderr.read_to_string(&mut buf) {
            Ok(_) => {
                if buf.is_empty() {
                    return;
                }
                error!("Language server error: {}", buf);
            }
            Err(e) => {
                error!("Failed to read from language server stderr: {}", e);
                return;
            }
        }
    });
    // XXX

    // NOTE 1024 is arbitrary
    let (reader_tx, reader_rx) = bounded(1024);
    let reader_thread = thread::spawn(move || {
        if let Err(msg) = reader_loop(reader, &reader_tx) {
            error!("{}", msg);
        }
        // NOTE prevent zombie
        debug!("Waiting for language server process end");
        if child.wait().is_err() {
            error!("Language server wasn't running was it?!");
        }

        let notification = jsonrpc_core::Notification {
            jsonrpc: Some(Version::V2),
            method: notification::Exit::METHOD.to_string(),
            params: Some(Params::None),
        };
        debug!("Sending exit notification back to controller");
        reader_tx.send(ServerMessage::Request(Call::Notification(notification)));
    });

    // NOTE 1024 is arbitrary
    let (writer_tx, writer_rx): (Sender<ServerMessage>, Receiver<ServerMessage>) = bounded(1024);
    let writer_thread = thread::spawn(move || {
        if writer_loop(writer, &writer_rx).is_err() {
            error!("Failed to write message to language server");
        }
        // NOTE we rely on assumption that if write failed then read is failed as well
        // or will fail shortly and do all exiting stuff
    });

    let rendezvous = thread::spawn(move || {
        if error_thread.join().is_err() {
            error!("Language server error monitoring thread panicked");
        };
        if reader_thread.join().is_err() {
            error!("Language server reader thread panicked");
        };
        if writer_thread.join().is_err() {
            error!("Language server writer thread panicked");
        };
    });

    LanguageServerTransport {
        sender: writer_tx,
        receiver: reader_rx,
        thread: rendezvous,
    }
}

fn reader_loop(mut reader: impl BufRead, tx: &Sender<ServerMessage>) -> io::Result<()> {
    let mut headers = FnvHashMap::default();
    loop {
        headers.clear();
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                debug!("Language server closed pipe, stopping reading");
                return Ok(());
            }
            let header = header.trim();
            if header.is_empty() {
                break;
            }
            let parts: Vec<&str> = header.split(": ").collect();
            if parts.len() != 2 {
                return Err(Error::new(ErrorKind::Other, "Failed to parse header"));
            }
            headers.insert(parts[0].to_string(), parts[1].to_string());
        }
        let content_len = headers
            .get("Content-Length")
            .ok_or_else(|| Error::new(ErrorKind::Other, "Failed to get Content-Length header"))?
            .parse()
            .map_err(|_| Error::new(ErrorKind::Other, "Failed to parse Content-Length header"))?;
        let mut content = vec![0; content_len];
        reader.read_exact(&mut content)?;
        let msg = String::from_utf8(content)
            .map_err(|_| Error::new(ErrorKind::Other, "Failed to read content as UTF-8 string"))?;
        debug!("From server: {}", msg);
        let output: serde_json::Result<Output> = serde_json::from_str(&msg);
        match output {
            Ok(output) => tx.send(ServerMessage::Response(output)),
            Err(_) => {
                let msg: Call = serde_json::from_str(&msg).map_err(|_| {
                    Error::new(ErrorKind::Other, "Failed to parse language server message")
                })?;
                tx.send(ServerMessage::Request(msg));
            }
        }
    }
}

fn writer_loop(mut writer: impl Write, rx: &Receiver<ServerMessage>) -> io::Result<()> {
    for request in rx {
        let request = match request {
            ServerMessage::Request(request) => serde_json::to_string(&request),
            ServerMessage::Response(response) => serde_json::to_string(&response),
        }?;
        debug!("To server: {}", request);
        write!(
            writer,
            "Content-Length: {}\r\n\r\n{}",
            request.len(),
            request
        )?;
        writer.flush()?;
    }
    // NOTE we rely on the assumption that language server will exit when its stdin is closed
    // without need to kill child process
    debug!("Received signal to stop language server, closing pipe");
    Ok(())
}
