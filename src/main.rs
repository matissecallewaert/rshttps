use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::sync::mpsc::channel;
use clap::Parser;

type FileCache = Arc<RwLock<HashMap<String, (Vec<u8>, String)>>>;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Port to serve on
    #[arg(short, long, default_value = "8000")]
    port: u16,
}
fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    let address = format!("127.0.0.1:{}", cli.port);

    match std::net::TcpListener::bind(&address) {
        Ok(listener) => {
            println!("Serving HTTP on {} ...", address);
            let current_dir = Arc::new(std::env::current_dir()?);
            let cache: FileCache = Arc::new(RwLock::new(HashMap::new()));

            let cache_clone = Arc::clone(&cache);
            let current_dir_clone = Arc::clone(&current_dir);

            thread::spawn(move || {
                setup_file_watcher(current_dir_clone, cache_clone);
            });

            for stream in listener.incoming() {
                let stream = stream?;
                let current_dir = Arc::clone(&current_dir);
                let cache = Arc::clone(&cache);

                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, &current_dir, cache) {
                        if e.kind() != std::io::ErrorKind::BrokenPipe {
                            eprintln!("Error handling client: {}", e);
                        }
                    }                    
                });
            }
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                eprintln!("Error: The address {} is already in use.", address);
            } else {
                eprintln!("Failed to bind to address {}: {}", address, e);
            }
        }
    }

    Ok(())
}

/// Set up the file watcher and invalidate the cache on file changes
fn setup_file_watcher(base_dir: Arc<PathBuf>, cache: FileCache) {
    let (tx, rx) = channel();
    let mut watcher = RecommendedWatcher::new(tx, Config::default()).expect("Failed to create watcher");
    watcher.watch(&*base_dir, RecursiveMode::Recursive).expect("Failed to watch the directory");

    let mut last_event_time: HashMap<String, Instant> = HashMap::new();

    for event in rx {
        match event {
            Ok(Event {
                kind: EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_),
                paths,
                ..
            }) => {
                let mut cache_guard = cache.write().unwrap();
                for path in paths {
                    if let Some(extension) = path.extension() {
                        if extension == "html" || extension == "css" || extension == "js" {
                            let relative_path = format!("/{}", path.strip_prefix(&*base_dir).unwrap_or(&path).to_string_lossy());
                            let now = Instant::now();

                            // Check if we recently processed this file
                            if let Some(last_time) = last_event_time.get(&relative_path) {
                                if now.duration_since(*last_time) < Duration::from_millis(200) {
                                    continue; // Skip this event
                                }
                            }

                            // Update the last event time
                            last_event_time.insert(relative_path.clone(), now);

                            println!("File change detected: {:?}", path);
                            println!("Removing cache entry: {:?}", relative_path);
                            cache_guard.remove(&relative_path);
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("Watch error: {:?}", e),
        }
    }
}

/// Handles incoming HTTP requests
fn handle_client(
    mut stream: std::net::TcpStream,
    base_dir: &Path,
    cache: FileCache,
) -> std::io::Result<()> {
    let mut buffer = Vec::new(); // Dynamic buffer
    let mut temp_buffer = [0; 1024];

    loop {
        let bytes_read = stream.read(&mut temp_buffer)?;

        if bytes_read == 0 {
            break;
        }

        buffer.extend_from_slice(&temp_buffer[..bytes_read]);

        // Check for the end of the request
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let request = String::from_utf8_lossy(&buffer);
    let first_line = request.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let mut path = parts.next().unwrap_or("/");

    println!("Method: {}, File requested: {}", method, path);

    if method != "GET" {
        return respond_with_error(&mut stream, 405, "Method Not Allowed");
    }

    path = if path == "/" { "/index.html" } else { path };

    {
        let cache_guard = cache.read().unwrap();
        if let Some((contents, mime_type)) = cache_guard.get(path) {
            println!("Serving from cache: {}", path);
            return respond_with_file(&mut stream, contents, mime_type);
        }
    }

    let file_path = base_dir.join(&path[1..]); // Remove leading '/'
    
    if file_path.exists() && file_path.is_file() {
        let contents = fs::read(&file_path)?;
        let mime_type = mime_guess::from_path(&file_path).first_or_octet_stream().to_string();

        let mut cache_guard = cache.write().unwrap();
        cache_guard.insert(path.to_string(), (contents.clone(), mime_type.clone()));
        
        return respond_with_file(&mut stream, &contents, &mime_type);
    } else {
        respond_with_error(&mut stream, 404, "Not Found")
    }
}

/// Sends a file as an HTTP response
fn respond_with_file(
    stream: &mut std::net::TcpStream,
    contents: &[u8],
    mime_type: &str,
) -> std::io::Result<()> {
    let content_type_header = if mime_type == "application/octet-stream" {
        "".to_string() // No header for unknown MIME types
    } else {
        format!("Content-Type: {}\r\n\r\n", mime_type)
    };

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{}",
        contents.len(),
        content_type_header
    );

    stream.write_all(header.as_bytes())?;
    stream.write_all(contents)?;
    stream.flush()
}

/// Sends an HTTP error response
fn respond_with_error(
    stream: &mut std::net::TcpStream,
    code: u16,
    message: &str,
) -> std::io::Result<()> {
    let body = format!("<h1>{} {}</h1>", code, message);
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: text/html\r\n\r\n",
        code, message, body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}
