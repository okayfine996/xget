use std::sync::{Arc, Condvar, Mutex};
use std::sync::mpsc::channel;
use std::thread;
use clap::Parser;
use reqwest::Error;
use xget::http;
use xget::http::fetcher::{Fetcher, SharedData};

fn main() {
    env_logger::init();

    let cli = Cli::parse();

    let shared_data = Arc::new(Mutex::new(SharedData { exit_flag: false }));
    let mut fetcher = Fetcher::new(cli.url, cli.output, cli.connections, shared_data.clone());
    match fetcher.resolve() {
        Ok(_) => {}
        Err(err) => panic!("{}", err)
    }


    let sha = shared_data.clone();
    thread::spawn(move || {
        let (tx, rx) = channel();
        ctrlc::set_handler(move || tx.send(()).expect("Could not send signal on channel."))
            .expect("Error setting Ctrl-C handler");

        println!("Waiting for Ctrl-C...");
        rx.recv().expect("Could not receive from channel.");
        println!("Got it! Exiting...");
        let mut data = sha.lock().unwrap();
        data.exit_flag = true;
    });


    fetcher.start_download();
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


