use std::fmt;

#[derive(Debug)]
pub struct ConnectionError {
    details: String,
}

impl ConnectionError {
    pub fn new(msg: &str) -> ConnectionError {
        ConnectionError { details: msg.to_string() }
    }
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.details)
    }
}

impl std::error::Error for ConnectionError {}
