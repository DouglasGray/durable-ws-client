use std::time::Duration;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

#[derive(Debug, Clone, Copy)]
pub struct ConfigBuilder {
    ws_config: Option<WebSocketConfig>,
    connect_timeout: Duration,
    close_timeout: Duration,
    channel_size: usize,
    disable_nagle: bool,
}

impl ConfigBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ws_config(&mut self, config: WebSocketConfig) -> &mut Self {
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
            ws_config: self.ws_config,
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
#[derive(Debug, Clone, Copy)]
pub struct Config {
    ws_config: Option<WebSocketConfig>,
    connect_timeout: Duration,
    close_timeout: Duration,
    channel_size: usize,
    disable_nagle: bool,
}

impl Config {
    pub fn ws_config(&self) -> Option<WebSocketConfig> {
        self.ws_config
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
