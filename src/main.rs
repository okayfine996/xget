use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::sync::mpsc::channel;
use std::thread;
use clap::Parser;
use reqwest::Error;
use xget::http;
use xget::http::fetcher;
use xget::http::fetcher::{extract_filename_from_url, Fetcher, FetcherState, SharedData};

fn main() {
    env_logger::init();

    let cli = Cli::parse();

    let mut fetcher: Fetcher;

    let url = cli.url.clone() + ".tmp";

    let file = extract_filename_from_url(&url).unwrap();
    if Path::new(file.as_str()).exists() {
        let file = File::open(file).unwrap();
        let reader = BufReader::new(file);
        fetcher = serde_json::from_reader(reader).unwrap();
    }else {
        fetcher = Fetcher::new(cli.url, cli.output, cli.connections);
    }

    let shared_data = Arc::new(Mutex::new(SharedData { exit_flag: false }));
    if fetcher.state == FetcherState::WaitStart {
        match fetcher.resolve() {
            Ok(_) => {}
            Err(err) => panic!("{}", err)
        }
    }

    let share_data = shared_data.clone();
    thread::spawn(move || {
        let (tx, rx) = channel();
        ctrlc::set_handler(move || tx.send(()).expect("Could not send signal on channel."))
            .expect("Error setting Ctrl-C handler");

        println!("Waiting for Ctrl-C...");
        rx.recv().expect("Could not receive from channel.");
        println!("Got it! Exiting...");
        let mut data = share_data.lock().unwrap();
        data.exit_flag = true;
    });


    fetcher.start_download(shared_data.clone());

    if fetcher.state == FetcherState::Paused {
        let mut temp_file = File::create(fetcher.output_path.clone().unwrap() + ".tmp").unwrap();
        serde_json::to_writer(&temp_file, &fetcher);
    }
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// File url
    url: String,

    /// File output directory, default is current directory.
    #[arg(short, long)]
    output: Option<String>,

    /// Nums of tcp connections, Default is twice the number of cpu cores
    #[arg(short, long)]
    connections: Option<usize>,
}


