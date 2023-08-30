use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::AsRawFd,
};

use runtime::*;

pub fn main() {
    SCHEDULER.spawn(listen());
    SCHEDULER.run();
}

fn listen() -> impl Future<Output = ()> {
    poll_fn(|waker| {
        let listener = TcpListener::bind("localhost:3000").unwrap();

        listener.set_nonblocking(true).unwrap();

        REACTOR.with(|reactor| {
            reactor.add(listener.as_raw_fd(), waker);
        });

        Some(listener)
    })
    .chain(|listener| {
        poll_fn(move |_| match listener.accept() {
            Ok((connection, _)) => {
                connection.set_nonblocking(true).unwrap();
                SCHEDULER.spawn(handle(connection));
                // `accept` always returns "None" because we want it to be
                // called every time a new connection comes in. It runs for the
                // entirety of our program.
                None
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => None,
            Err(e) => panic!("{e}"),
        })
    })
}

fn handle(connection: TcpStream) -> impl Future<Output = ()> {
    let mut connection = Some(connection);
    poll_fn(move |waker| {
        REACTOR.with(|reactor| {
            reactor.add(connection.as_ref().unwrap().as_raw_fd(), waker);
        });

        Some(connection.take())
    })
    .chain(move |mut connection| {
        let mut request = [0u8; 1024];
        let mut read = 0;

        poll_fn(move |_| {
            loop {
                // Try reading from the stream.
                match connection.as_mut().unwrap().read(&mut request) {
                    Ok(0) => {
                        println!("client disconnected unexpectedly");
                        return Some(connection.take());
                    }
                    Ok(n) => read += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        return None
                    }
                    Err(e) => panic!("{e}"),
                }

                // Did we reach the end of the request?
                let read = read;
                if read >= 4 && &request[read - 4..read] == b"\r\n\r\n" {
                    break;
                }
            }

            // We're done, print the request
            let request = String::from_utf8_lossy(&request[..read]);
            println!("{request}");

            Some(connection.take())
        })
    })
    .chain(move |mut connection| {
        let response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Length: 12\n",
            "Connection: close\r\n\r\n",
            "Hello world!"
        );
        let mut written = 0;

        poll_fn(move |_| {
            loop {
                match connection
                    .as_mut()
                    .unwrap()
                    .write(&response.as_bytes()[written..])
                {
                    Ok(0) => {
                        println!("client disconnected unexpectedly");
                        return Some(connection.take());
                    }
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        return None
                    }
                    Err(e) => panic!("{e}"),
                }

                // Did we write the whole response yet?
                if written == response.len() {
                    break;
                }
            }

            Some(connection.take())
        })
    })
    .chain(move |mut connection| {
        poll_fn(move |_| {
            match connection.as_mut().unwrap().flush() {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return None;
                }
                Err(e) => panic!("{e}"),
            };

            REACTOR.with(|reactor| {
                reactor.remove(connection.as_ref().unwrap().as_raw_fd());
            });

            Some(())
        })
    })
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

    #[derive(Clone)]
    pub struct Waker(Arc<dyn Fn() + Send + Sync>);

    impl Waker {
        pub fn wake(&self) {
            (self.0)()
        }
    }

    pub trait Future {
        type Output;

        fn poll(&mut self, waker: Waker) -> Option<Self::Output>;

        // Chain a closure over the first future's output. That way the closure
        // can use the output of the first future to construct the second.
        fn chain<F, T>(self, transition: F) -> Chain<Self, F, T>
        where
            F: FnOnce(Self::Output) -> T,
            T: Future,
            Self: Sized,
        {
            Chain::First {
                future1: self,
                transition: Some(transition),
            }
        }
    }

    // The Chain future is a generalization of our state machines, so it itself
    // is a mini state machine. It starts off by polling the first future,
    // holding onto the transition closure for when it finishes.
    pub enum Chain<T1, F, T2> {
        First { future1: T1, transition: Option<F> },
        Second { future2: T2 },
    }

    impl<T1, F, T2> Future for Chain<T1, F, T2>
    where
        T1: Future,
        F: FnOnce(T1::Output) -> T2,
        T2: Future,
    {
        type Output = T2::Output;

        // Notice how the same waker is used to poll both _futures_. This means
        // that notifications for both _futures_ will be propagated to the
        // Chain parent _future_, depending on its state.
        fn poll(&mut self, waker: Waker) -> Option<Self::Output> {
            if let Chain::First {
                future1,
                transition,
            } = self
            {
                // Poll the first future.
                match future1.poll(waker.clone()) {
                    Some(value) => {
                        // First future is done, transition into the second.
                        let future2 = (transition.take().unwrap())(value);
                        *self = Chain::Second { future2 };
                    }
                    // First future is not ready, return.
                    None => return None,
                }
            }

            if let Chain::Second { future2 } = self {
                // First future is already done, poll the second.
                return future2.poll(waker);
            }

            None
        }
    }

    // Create a _future_ from some block of code, and a way to combine two
    // futures, chaining them together.
    pub fn poll_fn<F, T>(f: F) -> impl Future<Output = T>
    where
        F: FnMut(Waker) -> Option<T>,
    {
        // A wrapper struct that implements "Future" and delegates poll to the
        // closure.
        struct PollFn<F>(F);

        impl<F, T> Future for PollFn<F>
        where
            F: FnMut(Waker) -> Option<T>,
        {
            type Output = T;

            fn poll(&mut self, waker: Waker) -> Option<Self::Output> {
                (self.0)(waker)
            }
        }

        PollFn(f)
    }

    pub static SCHEDULER: Scheduler = Scheduler {
        runnable: Mutex::new(VecDeque::new()),
    };

    // The scheduler.
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
                    // `pop` a runnable task off the queue.
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

                    // Poll the task.
                    task.lock().unwrap().poll(Waker(wake));
                }

                // If there are no runnable tasks, block on epoll until
                // something becomes ready.
                REACTOR.with(|reactor| reactor.wait());
            }
        }
    }

    thread_local! {
        pub static REACTOR: Reactor = Reactor::new();
    }

    // The reactor.
    pub struct Reactor {
        epoll: RawFd,
        tasks: RefCell<HashMap<RawFd, Waker>>,
    }

    impl Reactor {
        pub fn new() -> Reactor {
            Reactor {
                epoll: epoll::create(false).unwrap(),
                tasks: RefCell::new(HashMap::new()),
            }
        }

        pub fn add(&self, fd: RawFd, waker: Waker) {
            let event = epoll::Event::new(
                Events::EPOLLIN | Events::EPOLLOUT,
                fd as u64,
            );
            epoll::ctl(self.epoll, EPOLL_CTL_ADD, fd, event).unwrap();
            self.tasks.borrow_mut().insert(fd, waker);
        }

        pub fn remove(&self, fd: RawFd) {
            self.tasks.borrow_mut().remove(&fd);
        }

        pub fn wait(&self) {
            let mut events = [Event::new(Events::empty(), 0); 1024];
            let timeout = -1; // forever
            let num_events =
                epoll::wait(self.epoll, timeout, &mut events).unwrap();

            for event in &events[..num_events] {
                let fd = event.data as i32;
                let tasks = self.tasks.borrow_mut();

                // Notify the task.
                if let Some(waker) = tasks.get(&fd) {
                    waker.wake();
                }
            }
        }
    }
}
