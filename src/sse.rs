//! 增量解析 Gemini SSE，支持任意网络分段、CRLF 和有界事件缓存

use serde_json::Value;

use crate::error::{Error, Result};

const MAX_EVENT_BYTES: usize = 512 * 1024;

pub(crate) struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
    skip_lf: bool,
    first_line: bool,
    done: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            skip_lf: false,
            first_line: true,
            done: false,
        }
    }
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        for &byte in bytes {
            if std::mem::take(&mut self.skip_lf) && byte == b'\n' {
                continue;
            }
            if matches!(byte, b'\n' | b'\r') {
                self.skip_lf = byte == b'\r';
                self.line(&mut values)?;
            } else {
                if self.line.len() + self.data.len() >= MAX_EVENT_BYTES {
                    return Err(Error::protocol("Gemini SSE event exceeds its size limit"));
                }
                self.line.push(byte);
            }
        }
        Ok(values)
    }

    fn line(&mut self, values: &mut Vec<Value>) -> Result<()> {
        let raw = std::mem::take(&mut self.line);
        let line = if std::mem::take(&mut self.first_line) {
            raw.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&raw)
        } else {
            &raw
        };
        if line.is_empty() {
            if self.data.is_empty() {
                return Ok(());
            }
            if self.done {
                return Err(Error::protocol("Gemini sent data after the SSE end marker"));
            }
            self.data.pop();
            if self.data == b"[DONE]" {
                self.done = true;
            } else {
                values.push(
                    serde_json::from_slice(&self.data)
                        .map_err(|_| Error::protocol("Gemini sent invalid SSE JSON"))?,
                );
            }
            self.data.clear();
        } else if let Some(data) = line
            .strip_prefix(b"data:")
            .map(|value| value.strip_prefix(b" ").unwrap_or(value))
            .or_else(|| (line == b"data").then_some(&[][..]))
        {
            if self.data.len() + data.len() + 1 > MAX_EVENT_BYTES {
                return Err(Error::protocol("Gemini SSE event exceeds its size limit"));
            }
            self.data.extend_from_slice(data);
            self.data.push(b'\n');
        }
        Ok(())
    }

    pub fn finish(&self) -> Result<()> {
        if !self.line.is_empty() || !self.data.is_empty() {
            return Err(Error::protocol("Gemini SSE ended in a partial event"));
        }
        Ok(())
    }
}
