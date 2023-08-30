use runtime::*;
use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::AsRawFd,
};

fn main() {
    // This task will be pushed on and off the scheduler's run queue for the
    // entirety of our program.
    enum Main {
        Start,
        // The main task will stay in the "Accept" state for the rest of the
        // program, attempting to accept new connections.
        Accept { listener: TcpListener },
    }

    impl Future for Main {
        type Output = ();

        fn poll(&mut self, waker: Waker) -> Option<()> {
            if let Main::Start = self {
                let listener = TcpListener::bind("localhost:3000").unwrap();
                listener.set_nonblocking(true).unwrap();

                // Register the listener with epoll.
                // Give the reactor the waker provided to us by the scheduler.
                // When a connection comes, epoll will return an event and the
                // Reactor will wake the task, causing the scheduler to push our
                // task back onto the queue and poll us again.
                // The waker keeps everything connected.
                REACTOR.with(|reactor| {
                    reactor.add(listener.as_raw_fd(), waker);
                });

                *self = Main::Accept { listener };
            }

            if let Main::Accept { listener } = self {
                match listener.accept() {
                    Ok((connection, _)) => {
                        // Set it to non-blocking mode.
                        connection.set_nonblocking(true).unwrap();
                        // Spawn a new task to handle the request.
                        SCHEDULER.spawn(Handler {
                            connection,
                            state: HandlerState::Start,
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        return None;
                    }
                    Err(e) => panic!("{e}"),
                }
            }

            // If the listener is not ready. This tells the scheduler the future
            // is not yet ready, and it will be rescheduled once the reactor
            // wakes us.
            None
        }
    }

    SCHEDULER.spawn(Main::Start);
    SCHEDULER.run();
}

// Manages the connection itself along with its current state.
struct Handler {
    connection: TcpStream,
    state: HandlerState,
}

enum HandlerState {
    Start,
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

impl Future for Handler {
    type Output = ();

    fn poll(&mut self, waker: Waker) -> Option<Self::Output> {
        if let HandlerState::Start = self.state {
            // Start by registering our connection for notifications. Pass the
            // waker so that the scheduler knows when to run it again.
            REACTOR.with(|reactor| {
                reactor.add(self.connection.as_raw_fd(), waker);
            });

            self.state = HandlerState::Read {
                request: [0u8; 1024],
                read: 0,
            };
        }

        if let HandlerState::Read { request, read } = &mut self.state {
            loop {
                match self.connection.read(request) {
                    Ok(0) => {
                        println!("client disconnected unexpectedly");
                        return Some(());
                    }
                    Ok(n) => *read += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        return None
                    }
                    Err(e) => panic!("{e}"),
                }

                // Did we reach the end of the request?
                let read = *read;
                if read >= 4 && &request[read - 4..read] == b"\r\n\r\n" {
                    break;
                }
            }

            // We're done, print the request.
            let request = String::from_utf8_lossy(&request[..*read]);
            println!("{}", request);

            // Move into the write state.
            let response = concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Length: 12\n",
                "Connection: close\r\n\r\n",
                "Hello world!"
            );

            self.state = HandlerState::Write {
                response: response.as_bytes(),
                written: 0,
            };
        }

        // Write the response.
        if let HandlerState::Write { response, written } = &mut self.state {
            loop {
                match self.connection.write(&response[*written..]) {
                    Ok(0) => {
                        println!("client disconnected unexpectedly");
                        return Some(());
                    }
                    Ok(n) => *written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        return None
                    }
                    Err(e) => panic!("{e}"),
                }

                // Did we write the whole response yet?
                if *written == response.len() {
                    break;
                }
            }

            // Successfully wrote the response, try flushing next.
            self.state = HandlerState::Flush;
        }

        // Flush the response.
        if let HandlerState::Flush = self.state {
            match self.connection.flush() {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                Err(e) => panic!("{e}"),
            }
        }

        // Removes its connection from the reactor and returns `Some`. It will
        // never be run again after that point.
        REACTOR.with(|reactor| {
            reactor.remove(self.connection.as_raw_fd());
        });

        // Signifies that the _future_ has completed its work.
        Some(())
    }
}
mod runtime {
    use std::{
        cell::RefCell,
        collections::{HashMap, VecDeque},
        os::fd::RawFd,
        sync::{Arc, Mutex},
    };

    use epoll::Events;
    use epoll::{ControlOptions::EPOLL_CTL_ADD, Event};

    // A struct that holds a reference-counted, thread-safe pointer (Arc) to a
    // function (or closure) that:
    // - Takes no arguments.
    // - Can be safely transferred between threads (Send).
    // - Can be safely called from multiple threads at the same time (Sync).
    #[derive(Clone)]
    pub struct Waker(Arc<dyn Fn() + Send + Sync>);

    impl Waker {
        pub fn wake(&self) {
            (self.0)()
        }
    }

    // A task is really just a piece of code that needs to be run, representing
    // a value that will resolve sometime in the _future_.
    pub trait Future {
        type Output;

        // Run a future by asking it if the value is ready yet, _polling_ it,
        // and giving it a chance to make progress. Give each future a callback,
        // that when called, updates the scheduler's state for that _future_,
        // marking it as ready. That way our scheduler is completely
        // disconnected from epoll, or any other individual notification system.
        fn poll(&mut self, waker: Waker) -> Option<Self::Output>;
    }

    pub static SCHEDULER: Scheduler = Scheduler {
        runnable: Mutex::new(VecDeque::new()),
    };

    // Scheduler must be global and thread-safe because wakers are `Send`,
    // meaning `wake` may be called concurrently from other threads.
    #[derive(Default)]
    pub struct Scheduler {
        runnable: Mutex<VecDeque<SharedTask>>,
    }

    pub type SharedTask = Arc<Mutex<dyn Future<Output = ()> + Send>>;

    impl Scheduler {
        pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
            self.runnable
                .lock()
                .unwrap()
                .push_back(Arc::new(Mutex::new(task)));
        }

        pub fn run(&self) {
            loop {
                loop {
                    // Pop a runnable task off the queue.
                    let Some(task) = self.runnable.lock().unwrap().pop_front()
                    else {
                        break;
                    };
                    let t2 = task.clone();

                    // Create a waker that pushes the task back on.
                    let wake = Arc::new(move || {
                        SCHEDULER
                            .runnable
                            .lock()
                            .unwrap()
                            .push_back(t2.clone());
                    });

                    // poll the task
                    task.lock().unwrap().poll(Waker(wake));
                }

                // If there are no runnable tasks, block on `epoll` until
                // something becomes ready.
                REACTOR.with(|reactor| reactor.wait());
            }
        }
    }

    // A thread-local variable is one where each thread has its own, independent
    // version of the variable. Changes made to the variable in one thread won't
    // affect its value in other threads. This is important because the reactor
    // will be modified and accessed by different tasks throughout the program.
    thread_local! {
        // A background service that drives `epoll`, so we can register wakers
        // with it.
        pub static REACTOR: Reactor = Reactor::new();
    }

    // A simple object holding the epoll descriptor and a map of tasks keyed by
    // file descriptor.
    pub struct Reactor {
        epoll: RawFd,
        tasks: RefCell<HashMap<RawFd, Waker>>,
    }

    impl Reactor {
        pub fn new() -> Reactor {
            Reactor {
                epoll: epoll::create(false).unwrap(),
                // `RefCell` because the reactor will be modified and accessed
                // by different tasks throughout the program.
                tasks: RefCell::new(HashMap::new()),
            }
        }

        // Add a file descriptor with read and write interest.
        //
        // `waker` will be called when the descriptor becomes ready.
        pub fn add(&self, fd: RawFd, waker: Waker) {
            let event = epoll::Event::new(
                Events::EPOLLIN | Events::EPOLLOUT,
                fd as u64,
            );
            epoll::ctl(self.epoll, EPOLL_CTL_ADD, fd, event).unwrap();
            self.tasks.borrow_mut().insert(fd, waker);
        }

        // Remove the given descriptor from epoll.
        //
        // It will no longer recieve any notifications.
        pub fn remove(&self, fd: RawFd) {
            self.tasks.borrow_mut().remove(&fd);
        }

        // Drive tasks forward, blocking forever until an event arrives. All the
        // reactor has to do is wake the associated future for every event.
        // Remember, this will trigger the scheduler to run the _future_ later,
        // and continue the cycle.
        pub fn wait(&self) {
            let mut events = [Event::new(Events::empty(), 0); 1024];
            let timeout = -1; // forever
            let num_events =
                epoll::wait(self.epoll, timeout, &mut events).unwrap();

            for event in &events[..num_events] {
                let fd = event.data as i32;
                let tasks = self.tasks.borrow_mut();

                // notify the task
                if let Some(waker) = tasks.get(&fd) {
                    waker.wake();
                }
            }
        }
    }
}
