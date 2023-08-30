use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::TcpListener,
    os::fd::AsRawFd,
};

use epoll::{ControlOptions::*, Event, Events};

enum ConnectionState {
    Read {
        request: [u8; 1024],
        read: usize,
    },
    Write {
        response: &'static [u8],
        written: usize,
    },
    Flush,
}

pub fn main() {
    let listener = TcpListener::bind("localhost:3000").unwrap();
    listener.set_nonblocking(true).unwrap();

    // `epoll::create` returns a file descriptor that represents the newly
    // created epoll instance. It's a set of file descriptors that we can add or
    // remove from.
    let epoll = epoll::create(false).unwrap();

    // An `Event` has two parts, the interest flag, and the data field. The
    // interest flag gives us a way to tell epoll which I/O events we are
    // interested in. In the case of the TCP listener, we want to be notified
    // when new connections come in, so we pass the `EPOLLIN` flag. The data
    // field lets us store an ID that will uniquely identify each resource.
    // Remember, a file descriptor is a unique integer for a given file, so we
    // can just use that.
    let event = Event::new(Events::EPOLLIN, listener.as_raw_fd() as _);
    epoll::ctl(epoll, EPOLL_CTL_ADD, listener.as_raw_fd(), event).unwrap();

    // Now that we register connections, we'll get events for both the TCP
    // listener and individual connections. We need to store connections and
    // their states in a way that we can look up by file descriptor.
    let mut connections = HashMap::new();

    loop {
        let mut events = [Event::new(Events::empty(), 0); 1024];
        let timeout = -1;
        // `epoll::wait` accepts a list of events that it will populate with
        // information about the file descriptors that became ready.
        // It then returns the number of events that were added.
        // `epoll::wait` only blocks if there is nothing else to do. This idea
        // of blocking on multiple operations simultaneously is known as I/O
        // multiplexing.
        //
        // We **only ever do** I/O when epoll tells us to.
        let num_events = epoll::wait(epoll, timeout, &mut events).unwrap();

        let mut completed = Vec::new();
        'next: for event in &events[..num_events] {
            // The reason for using _i32_ for file descriptors is that in many
            // UNIX-like systems, file descriptors are non-negative integers.
            // Representing them as _i32_ might seem counter-intuitive because
            // they're non-negative, but it's a convention in many systems and
            // libraries because negative values are often used to indicate
            // errors or invalid descriptors.
            let fd = event.data as i32;

            // Use the file descriptor to check whether the event is for the TCP
            // listener, which means there's an incoming connection ready to
            // accept.
            if fd == listener.as_raw_fd() {
                // Try accepting a connection.
                match listener.accept() {
                    Ok((connection, _)) => {
                        connection.set_nonblocking(true).unwrap();

                        let fd = connection.as_raw_fd();

                        // Register the connection with epoll. Set both
                        // `EPOLLIN` and `EPOLLOUT`, because we are interested
                        // in both read and write events, depending on the
                        // state of the connection.
                        let event = Event::new(
                            Events::EPOLLIN | Events::EPOLLOUT,
                            fd as _,
                        );
                        epoll::ctl(epoll, EPOLL_CTL_ADD, fd, event).unwrap();

                        let state = ConnectionState::Read {
                            request: [0u8; 1024],
                            read: 0,
                        };

                        connections.insert(fd, (connection, state));
                    }
                    // If the call still returns `WouldBlock` for whatever
                    // reason, we can just move on and wait for the next event.
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(err) => panic!("failed to accept: {err}"),
                }

                continue 'next;
            }

            // epoll told us this connection is ready.
            let (connection, state) = connections.get_mut(&fd).unwrap();

            // No more _blocking_ IO on `connection.read`.
            if let ConnectionState::Read { request, read } = state {
                loop {
                    // try reading from the stream
                    match connection.read(&mut *request) {
                        Ok(0) => {
                            println!("client disconnected unexpectedly");
                            completed.push(fd);
                            continue 'next;
                        }
                        Ok(n) => {
                            // keep track of how many bytes we've read
                            *read += n;
                        }
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            // not ready yet, move on to the next connection
                            continue 'next;
                        }
                        Err(err) => panic!("{err}"),
                    }

                    // did we reach the end of the request?
                    if request.get(*read - 4..*read) == Some(b"\r\n\r\n") {
                        break;
                    }
                }

                // we're done, print the request
                let request = String::from_utf8_lossy(&request[..*read]);
                println!("{request}");
                // move into the write state
                let response = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Length: 12\n",
                    "Connection: close\r\n\r\n",
                    "Hello world!"
                );

                *state = ConnectionState::Write {
                    response: response.as_bytes(),
                    written: 0,
                };
            }

            // No more _blocking_ IO on `connection.write`.
            if let ConnectionState::Write { response, written } = state {
                loop {
                    match connection.write(&response[*written..]) {
                        Ok(0) => {
                            // client disconnected, mark this connection as complete
                            completed.push(fd);
                            continue 'next;
                        }
                        Ok(n) => {
                            *written += n;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            // not ready yet, move on to the next connection
                            continue 'next;
                        }
                        Err(e) => panic!("{e}"),
                    }

                    // did we write the whole response yet?
                    if *written == response.len() {
                        break;
                    }
                }

                // successfully wrote the response, try flushing next
                *state = ConnectionState::Flush;
            }
            // No more _blocking_ IO on `connection.flush`.
            if let ConnectionState::Flush = state {
                match connection.flush() {
                    Ok(_) => {
                        completed.push(fd);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // not ready yet, move on to the next connection
                        continue 'next;
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        }

        // Once we've finished reading, writing, and flushing the response, we
        // remove the connection from our map and drop it, which automatically
        // unregisters it from epoll.
        for fd in completed {
            let (connection, _state) = connections.remove(&fd).unwrap();
            drop(connection);
        }
    }
}
