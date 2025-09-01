use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(long = "host", short = 'H', default_value = "0.0.0.0")]
    pub host: String,

    #[arg(long = "port", short = 'P', default_value = "7000")]
    pub port: u16,

    #[arg(long = "connection-timeout", short = 't', default_value = "5")]
    pub connection_timeout: u64,

    #[arg(long = "idle-timeout", short = 'i', default_value = "0")]
    pub idle_timeout: u64,

    #[arg(long = "data-logging", short = 'd', default_value = "off")]
    pub data_logging: String,

    #[arg(long = "buffer-size", short = 'b', default_value = "32")]
    pub buffer_size: u32,
}
