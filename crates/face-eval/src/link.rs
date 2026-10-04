//! Serial link to a board running the `eval` firmware.

use std::io::{self, Read, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

use biometric_core::eval_protocol::{
    decode_response, Request, Response, CONSOLE_BAUD, MODEL_PREFIX, READY_PREFIX,
};
use serialport::{ClearBuffer, SerialPort};

/// Booting, verifying the models and their golden runs take a few seconds.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// Transfer of the largest image plus inference with two models.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
const ATTEMPTS: usize = 3;

pub struct Link {
    port: Box<dyn SerialPort>,
    pending: Vec<u8>,
    /// Feature models the board loaded, in the order of the embeddings it returns.
    pub models: Vec<String>,
}

/// What the ready line says: transfer baud rate and number of models.
fn parse_ready(line: &str) -> Option<(u32, usize)> {
    let rest = line.trim().strip_prefix(READY_PREFIX)?;
    let field = |key: &str| rest.split_whitespace().find_map(|f| f.strip_prefix(key));
    Some((
        field("baud=")?.parse().ok()?,
        field("models=")?.parse().ok()?,
    ))
}

/// A model line: index and model id.
fn parse_model(line: &str) -> Option<(usize, String)> {
    let rest = line.trim().strip_prefix(MODEL_PREFIX)?.trim_start();
    let (index, name) = rest.split_once(' ')?;
    Some((index.parse().ok()?, name.trim().to_owned()))
}

impl Link {
    /// Resets the board and waits until its evaluation server is ready.
    pub fn open(path: &str) -> io::Result<Self> {
        let mut port = serialport::new(path, CONSOLE_BAUD)
            .timeout(Duration::from_millis(200))
            .open()
            .map_err(io::Error::other)?;
        // The usual ESP auto-reset wiring: RTS pulls the chip's enable pin low. DTR stays
        // released so the chip boots the firmware and not the ROM downloader.
        port.write_data_terminal_ready(false)
            .map_err(io::Error::other)?;
        port.write_request_to_send(true).map_err(io::Error::other)?;
        sleep(Duration::from_millis(100));
        port.write_request_to_send(false)
            .map_err(io::Error::other)?;

        let mut link = Self {
            port,
            pending: Vec::new(),
            models: Vec::new(),
        };
        let deadline = Instant::now() + READY_TIMEOUT;
        let (baud, count) = loop {
            let line = link.read_line(deadline)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the board did not report `EVAL READY`; is the eval firmware flashed?",
                )
            })?;
            if let Some((index, name)) = parse_model(&line) {
                if index == link.models.len() {
                    link.models.push(name);
                }
            } else if let Some(ready) = parse_ready(&line) {
                break ready;
            }
        };
        if count != link.models.len() || count == 0 {
            return Err(io::Error::other(format!(
                "board announced {count} models but named {}",
                link.models.len()
            )));
        }
        // The board switches once the ready line has left its UART.
        sleep(Duration::from_millis(300));
        link.port.set_baud_rate(baud).map_err(io::Error::other)?;
        link.port
            .clear(ClearBuffer::Input)
            .map_err(io::Error::other)?;
        link.pending.clear();
        Ok(link)
    }

    /// Sends a packed B, G, R image and returns the board's answer. Transfer errors are retried.
    pub fn embed(&mut self, width: u16, height: u16, pixels: &[u8]) -> io::Result<Response> {
        let header = Request::for_image(width, height, pixels)
            .and_then(|request| request.encode())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut last = String::new();
        for _ in 0..ATTEMPTS {
            self.port.write_all(&header)?;
            self.port.write_all(pixels)?;
            self.port.flush()?;
            let deadline = Instant::now() + RESPONSE_TIMEOUT;
            last = loop {
                match self.read_line(deadline)? {
                    None => break "no response".to_owned(),
                    Some(line) => match decode_response(&line) {
                        None => continue, // log output
                        Some(Ok(Response::Error(reason))) => break format!("board: {reason}"),
                        Some(Ok(response)) => return Ok(response),
                        Some(Err(e)) => break format!("response: {e}"),
                    },
                }
            };
            // Let the board time out on a partial image before trying again.
            sleep(Duration::from_millis(500));
            self.port
                .clear(ClearBuffer::Input)
                .map_err(io::Error::other)?;
            self.pending.clear();
        }
        Err(io::Error::other(format!(
            "no valid answer after {ATTEMPTS} attempts (last: {last})"
        )))
    }

    /// The next line from the board, or `None` at `deadline`.
    fn read_line(&mut self, deadline: Instant) -> io::Result<Option<String>> {
        loop {
            if let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=end).collect();
                return Ok(Some(String::from_utf8_lossy(&line).trim_end().to_owned()));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            let mut chunk = [0u8; 4096];
            match self.port.read(&mut chunk) {
                Ok(n) => self.pending.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_line_is_parsed() {
        assert_eq!(
            parse_ready("EVAL READY baud=921600 models=2\r"),
            Some((921_600, 2))
        );
        assert_eq!(
            parse_ready("EVAL READY models=1 baud=115200"),
            Some((115_200, 1))
        );
        assert_eq!(parse_ready("EVAL READY baud=fast models=2"), None);
        assert_eq!(parse_ready("EVAL READY baud=921600"), None);
        assert_eq!(parse_ready("I (10) boot: ready"), None);
    }

    #[test]
    fn model_line_is_parsed() {
        assert_eq!(
            parse_model("EVAL MODEL 1 human_face_feat_mbf_s8_v1\r"),
            Some((1, "human_face_feat_mbf_s8_v1".to_owned()))
        );
        assert_eq!(parse_model("EVAL MODEL x name"), None);
        assert_eq!(parse_model("EVAL MODEL 1"), None);
        assert_eq!(parse_model("EVAL READY baud=1 models=1"), None);
    }
}
