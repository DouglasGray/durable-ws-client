use std::time::Duration;
use yawc::Options;

#[derive(Debug, Clone)]
pub struct ConfigBuilder {
    ws_config: Option<Options>,
    connect_timeout: Duration,
    close_timeout: Duration,
    channel_size: usize,
    disable_nagle: bool,
}

impl ConfigBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ws_config(&mut self, config: Options) -> &mut Self {
        self.ws_config = Some(config);
        self
    }

    pub fn connect_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.connect_timeout = timeout;
        self
    }

    pub fn close_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.close_timeout = timeout;
        self
    }

    pub fn channel_size(&mut self, size: usize) -> &mut Self {
        self.channel_size = size;
        self
    }

    pub fn disable_nagle(&mut self, disable_nagle: bool) -> &mut Self {
        self.disable_nagle = disable_nagle;
        self
    }

    pub fn build(&self) -> Config {
        Config {
            ws_config: self.ws_config.clone(),
            connect_timeout: self.connect_timeout,
            close_timeout: self.close_timeout,
            channel_size: self.channel_size,
            disable_nagle: self.disable_nagle,
        }
    }
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self {
            ws_config: None,
            connect_timeout: Duration::from_secs(10),
            close_timeout: Duration::from_secs(10),
            channel_size: 32,
            disable_nagle: false,
        }
    }
}

/// WebSocket client configuration.
///
/// `Options` is not `Copy`, so neither is this; it is cloned once per
/// connection attempt.
#[derive(Debug, Clone)]
pub struct Config {
    ws_config: Option<Options>,
    connect_timeout: Duration,
    close_timeout: Duration,
    channel_size: usize,
    disable_nagle: bool,
}

impl Config {
    pub fn ws_config(&self) -> Option<&Options> {
        self.ws_config.as_ref()
    }

    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub fn close_timeout(&self) -> Duration {
        self.close_timeout
    }

    pub fn channel_size(&self) -> usize {
        self.channel_size
    }

    pub fn disable_nagle(&self) -> bool {
        self.disable_nagle
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ws_config: None,
            connect_timeout: Duration::from_secs(10),
            close_timeout: Duration::from_secs(10),
            channel_size: 32,
            disable_nagle: false,
        }
    }
}
