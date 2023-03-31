use std::future::Future;
use std::io::{self, Read};
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
        loop {
            match self.stream.read(&mut self.buf) {
                Ok(0) => break,
                Ok(n) => self
                    .response
                    .push_str(&String::from_utf8_lossy(&self.buf[..n])),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Non-blocking operation is not ready yet.
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
        Poll::Ready(Ok(self.response.clone()))
    }

    // fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    //     let buf_size = 1024;
    //     let mut buf = [0; 1024];

    //     match &self.response_text {
    //         Some(response_text) => {
    //             // HTTPレスポンスがすでに受信されている場合は、そのまま返す
    //             Poll::Ready(Ok(response_text.clone()))
    //         }
    //         None => {
    //             // HTTPリクエストを送信
    //             let a = self.request_text.clone();
    //             let _ = self.socket.write_all(a.as_bytes());

    //             // HTTPレスポンスを受信
    //             match self.socket.read(&mut buf) {
    //                 Ok(n) if n > 0 => {
    //                     let response_text = std::str::from_utf8(&buf[..n])?.to_string();
    //                     self.response_text = Some(response_text.clone());
    //                     Poll::Ready(Ok(response_text))
    //                 }
    //                 Ok(_) => {
    //                     // ソケットがクローズされた場合はエラーを返す
    //                     Poll::Ready(Err("socket closed".into()))
    //                 }
    //                 Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
    //                     // リソースが利用できない場合はPendingを返す
    //                     cx.waker().wake_by_ref();
    //                     Poll::Pending
    //                 }
    //                 Err(e) => Poll::Ready(Err(e.into())),
    //             }
    //         }
    //     }
    // }
}

fn main() -> Result<(), Box<dyn Error>> {
    // HTTPリクエストを送信するサーバーのアドレスとポート番号
    let addr = "example.com:80";

    // HTTPリクエストのテキスト
    let request_text = format!("GET / HTTP/1.1\r\nHost: {}\r\n\r\n", addr);

    // TCPソケットを作成してサーバーに接続
    let socket = TcpStream::connect(addr)?;

    // HTTP GETリクエストを送信するFutureを作成
    let http_get_future = HttpGetFuture {
        socket: socket,
        request_text: request_text,
        response_text: None,
    };

    // Futureを実行
    let response_text = block_on(http_get_future)?;
    // HTTPレスポンスのボディを取得
    let body_start = response_text.find("\r\n\r\n").unwrap_or(0) + 4;
    let body_text = &response_text[body_start..];

    println!("{}", body_text);

    Ok(())
}
