use std::fs::File;
use std::io;
use std::ops::Index;
use std::os::unix::prelude::FileExt;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use reqwest::header::{ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, RANGE};
use reqwest::{Error, StatusCode, Url};
use threadpool::ThreadPool;
use regex::Regex;

#[derive(Debug, PartialEq, Eq)]
pub enum ChunkState {
    WaitStart,
    Downloading,
    Stop,
    Completed,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Chunk {
    pub state: ChunkState,
    pub begin: u64,
    pub end: u64,
}

impl Chunk {
    pub fn new(begin: u64, end: u64) -> Self {
        Chunk {
            state: ChunkState::WaitStart,
            begin,
            end,
        }
    }
}


enum FetcherState {
    WaitStart,
    Downloading,
    Paused,
    Completed,
    Failed,
}

struct Fetcher {
    state: FetcherState,
    chunks: Vec<Arc<Chunk>>,
    output: Option<Arc<Mutex<File>>>,
    url: String,
}

impl Fetcher {
    pub fn new(url: String) -> Fetcher {
        Fetcher {
            state: FetcherState::WaitStart,
            chunks: vec![],
            output: None,
            url,
        }
    }
}

const CHUNK_SIZE: u32 = 40960000;

impl Fetcher {
    pub fn resolve(&mut self) -> Result<(), Error> {
        let client = reqwest::blocking::Client::new();
        let result = client.get(&self.url).header(RANGE, 0).send()?;

        let content_size = result.headers().get(CONTENT_LENGTH).unwrap()
            .to_str().unwrap().parse::<u64>().unwrap();
        // support chunk
        if result.status().as_u16() == 206 || result.headers().get(ACCEPT_RANGES).unwrap().to_str().unwrap().contains("bytes") {
            self.chunks = split_chunk(content_size, CHUNK_SIZE as u64);
        } else {
            self.chunks.push(Arc::new(Chunk::new(0, content_size - 1)));
        }


        let mut fileName = String::new();

        println!("{:?}", result);

        if let op = result.headers().get(CONTENT_DISPOSITION) {
            if op.is_some() {
                let str = op.unwrap().to_str().unwrap().to_string();
                for str in str.split(';') {
                    if str.contains("filename") {
                        fileName = str.split('=').next().unwrap().to_string();
                        break
                    }
                }

            }
        }

        if fileName == "" {
            if let Some(file) = extract_filename_from_url(&self.url) {
               fileName = file;
            }else {
                fileName = "unknown".to_string();
            }
        }


        let mut file = File::create(fileName).unwrap();
        let _ = file.set_len(content_size);

        self.output = Some(Arc::new(Mutex::new(file)));

        Ok(())
    }



    pub fn start_download(&self) {

        let thread_pool = ThreadPool::new(32);

        for chunk in self.chunks.clone() {
            let file = self.output.clone().unwrap().clone();
            let url = self.url.clone();
            thread_pool.execute(move || {
                let client = reqwest::blocking::Client::new();
                let result = client.get(url).header(RANGE, format!("bytes={}-{}", chunk.begin, chunk.end)).send();

                match result {
                    Ok(result) => {
                        let bytes = result.bytes().unwrap();
                        let mut file = file.lock().unwrap();
                        let _ = file.write_all_at(bytes.as_ref(), chunk.begin);
                    }
                    Err(_) => {}
                }
            })
        }

        thread_pool.join();
    }
}

fn extract_filename_from_url(url: &str) -> Option<String> {
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


pub fn split_chunk(content_size: u64, chunk_size: u64) -> Vec<Arc<Chunk>> {
    let mut chunks = Vec::with_capacity((content_size / chunk_size + 1) as usize);

    if content_size < chunk_size {
        chunks.push(Arc::new(Chunk {
            state: ChunkState::WaitStart,
            begin: 0,
            end: content_size,
        }));
    }

    // the nums of chunk
    let nums = content_size / chunk_size;

    // split chunk
    for i in 0..nums {
        let begin = i * chunk_size as u64;
        let end = (i + 1) * chunk_size as u64 - 1;
        chunks.push(Arc::new(Chunk::new(begin, end)));
    }

    if content_size % chunk_size != 0 {
        chunks.push(Arc::new(Chunk::new(nums * chunk_size, content_size - 1)));
    }

    chunks
}



fn create_fixed_size_file(file: String, fixed_size: u64) -> Result<File, std::io::Error> {
    let mut file = File::create(file)?;
    let _ = file.set_len(fixed_size);
    Ok(file)
}

#[cfg(test)]
mod test {
    use crate::http::fetcher;
    use crate::http::fetcher::{Chunk, create_fixed_size_file, Fetcher, split_chunk};

    #[test]
    fn test_split_chunk() {
        let result = split_chunk(1096, 256);

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
        let mut fetcher = Fetcher::new("http://192.168.2.200:8080/go_admin.sql".to_string());
        let result = fetcher.resolve();
        fetcher.start_download();
    }
}