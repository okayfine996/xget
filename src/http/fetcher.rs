use std::fs::File;
use std::{error, io, thread};
use std::cell::RefCell;
use std::io::{Error, Read};
use std::ops::{Deref, Index};
use std::os::unix::prelude::FileExt;
use std::rc::Rc;
use std::sync::{Arc, Condvar, mpsc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, SyncSender};
use std::time::Duration;
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, trace};
use reqwest::header::{ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, RANGE};
use reqwest::{StatusCode, Url};
use threadpool::ThreadPool;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Clone)]
pub enum ChunkState {
    WaitStart,
    Downloading,
    Stop,
    Completed,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Clone)]
pub struct Chunk {
    pub state: ChunkState,
    pub begin: u64,
    pub end: u64,
    pub downloaded: u64,
    pub data: Vec<u8>,
}

impl Chunk {
    pub fn new(begin: u64, end: u64) -> Self {
        Chunk {
            state: ChunkState::WaitStart,
            begin,
            end,
            downloaded: 0,
            data: Vec::new(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FetcherState {
    WaitStart,
    Downloading,
    Paused,
    Completed,
    Failed,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fetcher {
    pub state: FetcherState,
    pub chunks: Vec<Chunk>,
    pub output_path: Option<String>,
    pub content_size: u64,
    pub url: String,
    pub connections: usize,
    pub downloaded: usize,
}

static counter: AtomicU64 = AtomicU64::new(0);

impl Fetcher {
    pub fn new(url: String, output: Option<String>, connections: Option<usize>) -> Fetcher {
        Fetcher {
            state: FetcherState::WaitStart,
            chunks: vec![],
            output_path: output,
            connections: connections.unwrap_or(num_cpus::get()),
            url,
            content_size: 0,
            downloaded: 0,
        }
    }
}

const CHUNK_SIZE: u32 = 40960000;

impl Fetcher {
    pub fn resolve(&mut self) -> Result<(), reqwest::Error> {
        let client = reqwest::blocking::Client::new();
        let result = client.get(&self.url).header(RANGE, 0).send()?;

        let content_size = result.headers().get(CONTENT_LENGTH).unwrap()
            .to_str().unwrap().parse::<u64>().unwrap();

        // check support chunk
        if result.status().is_success() && result.headers().get(ACCEPT_RANGES).unwrap().to_str().unwrap().contains("bytes") {
            for x in split_chunk(content_size, 16).into_iter() {
                self.chunks.push(x);
            }
        } else {
            self.chunks.push(Chunk::new(0, content_size - 1));
        }

        let mut fileName = String::new();
        if self.output_path.is_none() {
            debug!("{:?}", result);
            if let op = result.headers().get(CONTENT_DISPOSITION) {
                if op.is_some() {
                    let str = op.unwrap().to_str().unwrap().to_string();
                    for str in str.split(';') {
                        if str.contains("filename") {
                            fileName = str.split('=').next().unwrap().to_string();
                            break;
                        }
                    }
                }
            }

            if fileName == "" {
                if let Some(file) = extract_filename_from_url(&self.url) {
                    fileName = file;
                } else {
                    fileName = "unknown".to_string();
                }
            }
        }

        self.output_path = Some(fileName.clone());

        self.content_size = content_size;

        Ok(())
    }


    pub fn start_download(&mut self, share_data: Arc<Mutex<SharedData>>) {
        self.state = FetcherState::Downloading;

        let (tx, rx) = mpsc::channel();
        let mut handlers = vec![];

        // fetch chunk
        for chunk in self.chunks.to_vec() {
            let url = self.url.clone();
            let share_data = share_data.clone();
            let sender = tx.clone();
            let handler = thread::spawn(move || {
                fetch_chunk(sender, chunk, url, share_data)
            });

            handlers.push(handler);
        }

        drop(tx);

        // write data to file
        let output_path = self.output_path.clone().unwrap().clone();
        let file_size = self.content_size.clone();
        let download_size = self.downloaded.clone();

        let write_handler = thread::spawn(move || {
            let mut file = create_fixed_size_file(output_path, file_size).unwrap();

            let pb = ProgressBar::new(file_size);
            let style = ProgressStyle::default_bar().template("[{elapsed_precise}] {bar:40}  {bytes_per_sec} {percent}% {bytes}").unwrap().progress_chars("##-");
            pb.set_style(style);
            pb.set_position(download_size as u64);
            pb.reset_eta();


            for info in rx {
                let write_size = match file.write_at(&info.data, info.offset) {
                    Ok(size) => size,
                    Err(err) => {
                        debug!("{}",err);
                        0
                    }
                };

                pb.inc(write_size as u64);
            }

            pb.position()
        });

        self.chunks.clear();
        for handler in handlers {
            let chunk = handler.join().unwrap().unwrap();
            self.chunks.push(chunk);
        }

        self.downloaded = write_handler.join().unwrap() as usize;

        for x in &self.chunks {
            if x.state != ChunkState::Completed {
                self.state = FetcherState::Paused;
                break;
            }
        }
    }
}

fn fetch_chunk(tx: Sender<WriteInfo>, mut chunk: Chunk, url: String, share_data: Arc<Mutex<SharedData>>) -> Result<Chunk, Error> {
    if chunk.state == ChunkState::Completed {
        return Ok(chunk);
    }

    let client = reqwest::blocking::Client::new();

    // Try five times if there some thing wrong
    for i in 0..5 {
        let request = client.get(&url).header(RANGE, format!("bytes={}-{}", chunk.begin + chunk.downloaded, chunk.end));
        let mut result = request.send().unwrap();
        if !result.status().is_success() {
            thread::sleep(Duration::from_secs(5));
            continue;
        }

        // crate buffer
        let mut buffer = vec![0; 4096];
        loop {
            let offset = match result.read(&mut buffer) {
                Ok(offset) => offset,
                Err(_) => 0 as usize
            };

            if offset == 0 {
                break;
            }

            let send_result = tx.send(WriteInfo {
                offset: chunk.begin + chunk.downloaded,
                data: buffer[..offset].to_vec(),
            });

            if send_result.is_err() {
                println!("{:#?}", send_result.err().unwrap())
            }


            chunk.downloaded += offset as u64;

            let data = share_data.lock().unwrap();
            if data.exit_flag {
                return Ok(chunk);
            }
        }

        // Check chunk
        if chunk.end - chunk.begin + 1 == chunk.downloaded {
            println!("chunk {:?} download completed", &chunk);

            chunk.state = ChunkState::Completed;
            break;
        }
    }

    Ok(chunk)
}

#[derive(Debug)]
struct WriteInfo {
    offset: u64,
    data: Vec<u8>,
}


pub fn extract_filename_from_url(url: &str) -> Option<String> {
    // Parse the URL
    let parsed_url = Url::parse(url).ok()?;

    // Get the path component of the URL
    let path = parsed_url.path();

    // Use a regular expression to extract the filename
    let re = Regex::new(r"/([^/]+)$").unwrap();
    if let Some(captures) = re.captures(path) {
        return captures.get(1).map(|m| m.as_str().to_string());
    }

    None
}


pub fn split_chunk(content_size: u64, connections: u64) -> Vec<Chunk> {
    let mut chunks = Vec::with_capacity(connections as usize);

    let chunk_size = content_size / connections;

    // split chunk
    for i in 0..connections {
        let begin = i * chunk_size as u64;
        let mut end = (i + 1) * chunk_size as u64 - 1;
        if i == connections - 1 {
            end = content_size - 1
        }
        chunks.push(Chunk::new(begin, end));
    }

    chunks
}


fn create_fixed_size_file(file: String, fixed_size: u64) -> Result<File, std::io::Error> {
    let mut file = File::create(file)?;
    let _ = file.set_len(fixed_size);
    Ok(file)
}

pub struct SharedData {
    pub exit_flag: bool,
}

#[cfg(test)]
mod test {
    use std::sync::{Arc, Mutex};
    use crate::http::fetcher;
    use crate::http::fetcher::{Chunk, create_fixed_size_file, Fetcher, SharedData, split_chunk};

    #[test]
    fn test_split_chunk() {
        let result = split_chunk(1096, 5);

        let expect = vec![
            Chunk::new(0, 255),
            Chunk::new(256, 511),
            Chunk::new(512, 767),
            Chunk::new(768, 1023),
            Chunk::new(1024, 1095)];


        // assert_eq!(result, expect)
    }

    #[test]
    fn test_create_fixed_size_file() {
        let file = create_fixed_size_file("aaa.txt".to_string(), 102400).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 10240000);
    }

    #[test]
    fn test_download() {
        let mut fetcher = Fetcher::new("https://okg-pub-hk.oss-cn-hongkong.aliyuncs.com/cdn/okbc/snapshot/testnet-s0-20230720-2995040-rocksdb.tar.gz".to_string(),
                                       None, None);
        let shared_data = Arc::new(Mutex::new(SharedData { exit_flag: false }));
        let result = fetcher.resolve();
        fetcher.start_download(shared_data);
    }
}