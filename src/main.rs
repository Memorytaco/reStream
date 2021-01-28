#[macro_use]
extern crate anyhow;
extern crate lz_fear;

use anyhow::{Context, Result};
use clap::{crate_authors, crate_version, Clap};
use lz_fear::CompressionSettings;

use std::default::Default;
use std::fs::File;
use std::path::Path;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpStream, TcpListener};
use std::process::Command;
use std::time::Duration;
use std::thread;
use std::sync::mpsc::channel;

#[derive(Clap)]
#[clap(version = crate_version!(), author = crate_authors!())]
pub struct Opts {
    #[clap(
        long,
        name = "port",
        short = 'l',
        about = "Listen for an (unsecure) TCP connection to send the data to which reduces some load on the reMarkable and improves fps."
    )]
    listen: Option<usize>,
}

fn main() -> Result<()> {
    let ref opts: Opts = Opts::parse();

    let version = remarkable_version()?;
    let height = 1872;
    let streamer = if version == "reMarkable 1.0\n" {
        let width = 1408;
        let bytes_per_pixel = 2;
        ReStreamer::init("/dev/fb0", 0, width, height, bytes_per_pixel)?
    } else if version == "reMarkable 2.0\n" {
        let width = 1404;
        if Path::new("/dev/shm/swtfb.01").exists() {
            let bytes_per_pixel = 2;
            ReStreamer::init("/dev/shm/swtfb.01", 0, width, height, bytes_per_pixel)?
        } else {
            let bytes_per_pixel = 1;
            let pid = xochitl_pid()?;
            let offset = rm2_fb_offset(pid)?;
            let mem = format!("/proc/{}/mem", pid);
            ReStreamer::init(&mem, offset, width, height, bytes_per_pixel)?
        }
    } else {
        Err(anyhow!(
            "Unknown reMarkable version: {}\nPlease open a feature request to support your device.",
            version
        ))?
    };

    let stdout = std::io::stdout();
    let data_target: Box<dyn Write> = if let Some(port) = opts.listen {
        Box::new(listen_timeout(port, Duration::from_secs(3))?)
    } else {
        Box::new(stdout.lock())
    };

    let lz4: CompressionSettings = Default::default();
    lz4.compress(streamer, data_target)
        .context("Error while compressing framebuffer stream")
}

fn listen_timeout(port: usize, timeout: Duration) -> Result<TcpStream> {
    let listen_addr = format!("0.0.0.0:{}", port);
    let listen = TcpListener::bind(&listen_addr)?;
    eprintln!("[rM] listening for a TCP connection on {}", listen_addr);

    let (tx, rx) = channel();
    thread::spawn(move || {
        tx.send(listen.accept()).unwrap();
    });

    let (conn, conn_addr) = rx.recv_timeout(timeout)
        .context("Timeout while waiting for host to connect to reMarkable")??;
    eprintln!("[rM] connection received from {}", conn_addr);
    conn.set_write_timeout(Some(timeout))?;
    Ok(conn)
}

fn remarkable_version() -> Result<String> {
    let content = std::fs::read("/sys/devices/soc0/machine")
        .context("Failed to read /sys/devices/soc0/machine")?;
    Ok(String::from_utf8(content)?)
}

fn xochitl_pid() -> Result<usize> {
    let output = Command::new("/bin/pidof")
        .args(&["xochitl"])
        .output()
        .context("Failed to run `/bin/pidof xochitl`")?;
    if output.status.success() {
        let pid = &output.stdout;
        let pid_str = std::str::from_utf8(pid)?.trim();
        pid_str
            .parse()
            .with_context(|| format!("Failed to parse xochitl's pid: {}", pid_str))
    } else {
        Err(anyhow!(
            "Could not find pid of xochitl, is xochitl running?"
        ))
    }
}

fn rm2_fb_offset(pid: usize) -> Result<usize> {
    let file = File::open(format!("/proc/{}/maps", &pid))?;
    let line = BufReader::new(file)
        .lines()
        .skip_while(|line| matches!(line, Ok(l) if !l.ends_with("/dev/fb0")))
        .skip(1)
        .next()
        .with_context(|| format!("No line containing /dev/fb0 in /proc/{}/maps file", pid))?
        .with_context(|| format!("Error reading file /proc/{}/maps", pid))?;

    let addr = line
        .split("-")
        .next()
        .with_context(|| format!("Error parsing line in /proc/{}/maps", pid))?;

    let address = usize::from_str_radix(addr, 16).context("Error parsing framebuffer address")?;
    Ok(address + 8)
}

pub struct ReStreamer {
    file: File,
    start: u64,
    cursor: usize,
    size: usize,
}

impl ReStreamer {
    pub fn init(
        path: &str,
        offset: usize,
        width: usize,
        height: usize,
        bytes_per_pixel: usize,
    ) -> Result<ReStreamer> {
        let start = offset as u64;
        let size = width * height * bytes_per_pixel;
        let cursor = 0;
        let file = File::open(path)?;
        let mut streamer = ReStreamer {
            file,
            start: start,
            cursor,
            size,
        };
        streamer.next_frame()?;
        Ok(streamer)
    }

    pub fn next_frame(&mut self) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(self.start))?;
        self.cursor = 0;
        Ok(())
    }
}

impl Read for ReStreamer {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let requested = buf.len();
        let bytes_read = if self.cursor + requested < self.size {
            self.file.read(buf)?
        } else {
            let rest = self.size - self.cursor;
            self.file.read(&mut buf[0..rest])?
        };
        self.cursor += bytes_read;
        if self.cursor == self.size {
            self.next_frame()?;
        }
        Ok(bytes_read)
    }
}
