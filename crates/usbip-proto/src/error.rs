use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("short read: need {need} bytes, got {got}")]
    ShortRead { need: usize, got: usize },

    #[error("unknown op code: 0x{0:04x}")]
    UnknownOp(u16),

    #[error("unsupported protocol version: 0x{0:04x}")]
    UnsupportedVersion(u16),
}
