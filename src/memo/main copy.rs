use std::error::Error;
use std::future::Future;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::executor::block_on;
use url::Url;

struct HttpGetFuture {
    stream: TcpStream,
    url: Url,
    response: String,
    buf: [u8; 1024],
}

impl Future for HttpGetFuture {
    type Output = Result<String, Box<dyn Error>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        println!("poll");
        loop {
            println!("|");
            let mut a = self.buf.clone();
            match self.stream.read(&mut a) {
                Ok(0) => {
                    println!("break");
                    break;
                }
                Ok(n) => {
                    println!("{}", n);
                    self.response.push_str(&String::from_utf8_lossy(&a[..n]))
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Non-blocking operation is not ready yet.
                    println!("Non-blocking operation is not ready yet.");
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Err(e) => return Poll::Ready(Err(Box::new(e))),
            }
        }
        Poll::Ready(Ok(self.response.clone()))
    }
}

fn http_get(url: &Url) -> HttpGetFuture {
    let addr = url.socket_addrs(|| Some(8080)).unwrap();
    let mut stream =
        TcpStream::connect_timeout(addr.get(0).unwrap(), Duration::from_secs(5)).unwrap();
    stream.set_nonblocking(false).unwrap();
    stream
        .write_all(format!("GET {} HTTP/1.0\r\n\r\n", url.path()).as_bytes())
        .unwrap();
    HttpGetFuture {
        stream,
        url: url.clone(),
        response: String::new(),
        buf: [0; 1024],
    }
}

fn main() -> io::Result<()> {
    let url = Url::parse("http://127.0.0.1:8080/index.html").unwrap();
    let http_get_future = http_get(&url);
    let response_text = block_on(http_get_future);
    println!("{}", response_text.unwrap());
    Ok(())
}
