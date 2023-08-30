use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};

// $ curl localhost:3000
fn main() {
    // HTTP is a text-based protocol built on top of TCP, so we have to accept
    // TCP connections.
    let listener = TcpListener::bind("localhost:3000").unwrap();

    // Listen for incoming connections.
    loop {
        let (connection, _) = listener.accept().unwrap();

        if let Err(e) = handle_connection(connection) {
            println!("failed to handle connection: {e}")
        }
    }
}

/// Handles a TCP connection represented by a `TcpStream`.
///
/// The `TcpStream` is a bidirectional stream of data between the local host and
/// the client. This function abstracts away the details of handling the TCP
/// connection, leveraging the `Read` and `Write` trait implementations of the
/// `TcpStream` type to manage the data transfer.
///
/// # Arguments
///
/// * `connection` - The active TCP stream representing the client connection.
///
/// # Returns
///
/// An `io::Result<()>` indicating the success or failure of handling the
/// connection.
///
fn handle_connection(mut connection: TcpStream) -> io::Result<()> {
    let mut read = 0;
    // Initialize a small buffer to hold the request.
    let mut request = [0u8; 1024];

    loop {
        // Read the request bytes into the buffer. `Read` will fill the buffer
        // with an arbitrary number of bytes, not necessarily the entire request
        // at once. So we have to keep track of the total number of bytes we've
        // read and call it in a loop, reading the rest of the request into the
        // unfilled part of the buffer.
        let num_bytes = connection.read(&mut request[read..])?;

        // It's also possible for read to return zero bytes, which can happen
        // when the client disconnects.
        if num_bytes == 0 {
            println!("client disconnected");
            return Ok(());
        }

        read += num_bytes;

        // Check for the byte sequence "\r\n\r\n", which indicates the end of
        // the request.
        if request.get(read - 4..read) == Some(b"\r\n\r\n") {
            break;
        }
    }

    // Convert the request to a string and log it to the console.
    let request = String::from_utf8_lossy(&request[..read]);
    println!("{request}");

    // "Hello World" in HTTP.
    let response = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Length: 12\n",
        "Connection: close\r\n\r\n",
        "Hello world!"
    );

    let mut written = 0;

    loop {
        // A call to write may not write the entire buffer at once. We need a
        // second loop to ensure the entire response is written to the client,
        // with each call to write continuing from where the previous left off.
        let num_bytes = connection.write(response[written..].as_bytes())?;

        // Client disconnected.
        if num_bytes == 0 {
            println!("client disconnected");
            return Ok(());
        }

        written += num_bytes;

        // Have we written the whole response yet?
        if written == response.len() {
            break;
        }
    }

    // There's really no way to force flush a network socket so flush is
    // actually a no-op on `TcpStream`. Call it anyways to be true to io::Write.
    connection.flush()
}
