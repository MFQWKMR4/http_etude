use std::error::Error;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::pin::Pin;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::task::{Context, Poll};
use std::time::Duration;

use chrono::Local;
use rustyline::error::ReadlineError;
use rustyline::Editor;
use url::Url;

use futures::{
    future::{BoxFuture, FutureExt},
    task::{waker_ref, ArcWake},
};
use nix::{
    errno::Errno,
    sys::{
        epoll::{
            epoll_create1, epoll_ctl, epoll_wait, EpollCreateFlags, EpollEvent, EpollFlags, EpollOp,
        },
        eventfd::{eventfd, EfdFlags},
    },
    unistd::{read, write},
};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    os::unix::io::RawFd,
    sync::{Arc, Mutex},
    task::Waker,
};

struct HttpGetFuture {
    ed: Arc<EventDetector>,
    stream: TcpStream,
    response: String,
    buf: [u8; 1024],
}

fn write_eventfd(fd: RawFd, n: usize) {
    let ptr = &n as *const usize as *const u8;
    let val = unsafe { std::slice::from_raw_parts(ptr, std::mem::size_of_val(&n)) };
    write(fd, &val).unwrap();
}

enum EpollOps {
    ADD(EpollFlags, RawFd, Waker), // epollへ追加
    REMOVE(RawFd),                 // epollから削除
}

fn log_with_timestamp(message: &str) {
    let timestamp = Local::now();
    println!("[{}] {}", timestamp, message);
}

struct EventDetector {
    wakers: Mutex<HashMap<RawFd, Waker>>, // fdからwaker
    queue: Mutex<VecDeque<EpollOps>>,     // IOのキュー
    epfd: RawFd,                          // epollのfd
    event: RawFd,                         // eventfdのfd
}

impl EventDetector {
    fn new() -> Arc<Self> {
        let s = EventDetector {
            wakers: Mutex::new(HashMap::new()),
            queue: Mutex::new(VecDeque::new()),
            epfd: epoll_create1(EpollCreateFlags::empty()).unwrap(),
            event: eventfd(0, EfdFlags::empty()).unwrap(),
        };
        let result = Arc::new(s);
        let s = result.clone();
        std::thread::spawn(move || s.select());
        result
    }

    fn add_event(
        &self,
        flag: EpollFlags, // epollのフラグ
        fd: RawFd,        // 監視対象のファイルディスクリプタ
        waker: Waker,
        wakers: &mut HashMap<RawFd, Waker>,
    ) {
        let epoll_add = EpollOp::EpollCtlAdd;
        let epoll_mod = EpollOp::EpollCtlMod;
        let epoll_one = EpollFlags::EPOLLONESHOT;

        // EPOLLONESHOTを指定して、一度イベントが発生すると
        // そのfdへのイベントは再設定するまで通知されないようになる <5>
        let mut ev = EpollEvent::new(flag | epoll_one, fd as u64);
        // 監視対象に追加
        if let Err(err) = epoll_ctl(self.epfd, epoll_add, fd, &mut ev) {
            match err {
                nix::Error::Sys(Errno::EEXIST) => {
                    // すでに追加されていた場合は再設定 <6>
                    epoll_ctl(self.epfd, epoll_mod, fd, &mut ev).unwrap();
                }
                _ => {
                    panic!("epoll_ctl: {}", err);
                }
            }
        }
        log_with_timestamp(&format!("add_event: {:?}", ev));

        assert!(!wakers.contains_key(&fd));
        wakers.insert(fd, waker);
    }

    fn rm_event(&self, fd: RawFd, wakers: &mut HashMap<RawFd, Waker>) {
        let epoll_del = EpollOp::EpollCtlDel;
        let mut ev = EpollEvent::new(EpollFlags::empty(), fd as u64);
        epoll_ctl(self.epfd, epoll_del, fd, &mut ev).ok();
        wakers.remove(&fd);
    }

    fn select(&self) {
        let epoll_in = EpollFlags::EPOLLIN;
        let epoll_add = EpollOp::EpollCtlAdd;

        // eventfdをepollの監視対象に追加 <10>
        let mut ev = EpollEvent::new(epoll_in, self.event as u64);
        epoll_ctl(self.epfd, epoll_add, self.event, &mut ev).unwrap();

        let mut events = vec![EpollEvent::empty(); 1024];
        // event発生を監視
        while let Ok(nfds) = epoll_wait(
            self.epfd, // <11>
            &mut events,
            -1,
        ) {
            log_with_timestamp(&format!("nfds: {}", nfds));
            let mut t = self.wakers.lock().unwrap();
            for n in 0..nfds {
                if events[n].data() == self.event as u64 {
                    log_with_timestamp(&format!("eventfd"));
                    // eventfdの場合、追加、削除要求を処理 <12>
                    let mut q = self.queue.lock().unwrap();
                    while let Some(op) = q.pop_front() {
                        match op {
                            // 追加
                            EpollOps::ADD(flag, fd, waker) => {
                                self.add_event(flag, fd, waker, &mut t)
                            }
                            // 削除
                            EpollOps::REMOVE(fd) => self.rm_event(fd, &mut t),
                        }
                    }
                    let mut buf: [u8; 8] = [0; 8];
                    read(self.event, &mut buf).unwrap(); // eventfdの通知解除
                } else {
                    log_with_timestamp(&format!("not eventfd"));
                    // 実行キューに追加 <13>
                    let data = events[n].data() as i32;
                    let waker = t.remove(&data).unwrap();
                    waker.wake_by_ref();
                }
            }
        }
    }

    fn register(&self, flags: EpollFlags, fd: RawFd, waker: Waker) {
        let mut q = self.queue.lock().unwrap();
        q.push_back(EpollOps::ADD(flags, fd, waker));
        write_eventfd(self.event, 1);
    }

    fn unregister(&self, fd: RawFd) {
        let mut q = self.queue.lock().unwrap();
        q.push_back(EpollOps::REMOVE(fd));
        write_eventfd(self.event, 1);
    }
}

impl HttpGetFuture {
    fn new(url: &Url, ed: Arc<EventDetector>) -> Self {
        let addr = url.socket_addrs(|| Some(8080)).unwrap();
        let mut stream =
            TcpStream::connect_timeout(addr.get(0).unwrap(), Duration::from_secs(5)).unwrap();
        stream.set_nonblocking(true).unwrap();
        stream
            .write_all(format!("GET {} HTTP/1.0\r\n\r\n", url.path()).as_bytes())
            .unwrap();
        log_with_timestamp(&format!("request sent"));
        HttpGetFuture {
            ed,
            stream,
            response: String::new(),
            buf: [0; 1024],
        }
    }
}

impl Future for HttpGetFuture {
    type Output = Result<String, Box<dyn Error>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        log_with_timestamp(&format!("poll"));
        loop {
            log_with_timestamp(&format!("|"));
            let mut a = self.buf.clone();
            match self.stream.read(&mut a) {
                Ok(0) => {
                    log_with_timestamp(&format!("break"));
                    break;
                }
                Ok(n) => {
                    log_with_timestamp(&format!("{}", n));
                    self.response.push_str(&String::from_utf8_lossy(&a[..n]))
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    log_with_timestamp(&format!("Non-blocking operation is not ready yet."));
                    self.ed.register(
                        EpollFlags::EPOLLRDHUP | EpollFlags::EPOLLIN | EpollFlags::EPOLLONESHOT,
                        self.stream.as_raw_fd(),
                        cx.waker().clone(),
                    );
                    return Poll::Pending;
                }
                Err(e) => return Poll::Ready(Err(Box::new(e))),
            }
        }
        Poll::Ready(Ok(self.response.clone()))
    }
}

struct Task {
    future: Mutex<BoxFuture<'static, ()>>,
    sender: SyncSender<Arc<Task>>,
}

impl ArcWake for Task {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let self0 = arc_self.clone();
        arc_self.sender.send(self0).unwrap();
    }
}

struct Spawner {
    sender: SyncSender<Arc<Task>>,
}

impl Spawner {
    fn spawn(&self, future: impl Future<Output = ()> + 'static + Send) {
        let future = future.boxed();
        let task = Arc::new(Task {
            future: Mutex::new(future),
            sender: self.sender.clone(),
        });
        self.sender.send(task).unwrap();
    }
}

struct Executor {
    sender: SyncSender<Arc<Task>>,
    receiver: Receiver<Arc<Task>>,
}

impl Executor {
    fn new() -> Self {
        let (sender, receiver) = sync_channel(1024);
        Executor {
            sender: sender.clone(),
            receiver,
        }
    }

    fn get_spawner(&self) -> Spawner {
        Spawner {
            sender: self.sender.clone(),
        }
    }

    fn run(&self) {
        log_with_timestamp(&format!("run"));
        while let Ok(task) = self.receiver.recv() {
            log_with_timestamp(&format!("run..."));
            let mut future = task.future.lock().unwrap();
            let waker = waker_ref(&task);
            let mut ctx = Context::from_waker(&waker);
            let _ = future.as_mut().poll(&mut ctx);
        }
    }
}

async fn http_request(url: Url, event_detector: Arc<EventDetector>) {
    let res = HttpGetFuture::new(&url, event_detector).await;
    log_with_timestamp(&format!("response: {}", res.unwrap()));
}

fn main() -> io::Result<()> {
    let executor = Executor::new();
    let event_detector = EventDetector::new();

    let spawner = executor.get_spawner();
    let mut rl = Editor::<()>::new().unwrap();

    let exec_thread = std::thread::spawn(move || executor.run());
    let req_thread = std::thread::spawn(move || loop {
        match rl.readline(&format!("input request number: ")) {
            Ok(line) => {
                let line_trimed = line.trim();
                if line_trimed.is_empty() {
                    continue;
                }

                let input = line_trimed.parse::<i32>().unwrap();
                // let input = 3;
                for _ in 0..input {
                    spawner.spawn(http_request(
                        Url::parse("http://127.0.0.1:8080/index.html").unwrap(),
                        event_detector.clone(),
                    ));
                }
            }
            Err(ReadlineError::Interrupted) => eprintln!("info: Ctrl+D to exit"),
            Err(e) => {
                eprintln!("info: failed to load\n{e}");
                break;
            }
        }
    });

    req_thread.join().unwrap();
    exec_thread.join().unwrap();
    Ok(())
}
