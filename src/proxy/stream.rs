use futures::{Future, Poll};
use tokio_io::{AsyncRead, AsyncWrite, IoFuture};
use tokio_core::net::TcpStream;
use tokio_core::reactor::Remote;
use std::net::Shutdown;
use tokio_dns::tcp_connect;
use std::sync::Arc;
use std::io::{Error as IoError, ErrorKind as IoErrorKind, Read, Result as IoResult, Write};
use config::{Proxy, Tunnel};
use std::fmt::Debug;


#[derive(Clone)]
pub struct ProxyTcpStream {
    inner: Arc<TcpStream>,
    is_proxied: bool,
}

fn read_proxy_response(s: ProxyTcpStream) -> ConnectResponse {
    ConnectResponse {
        stream: Some(s),
        status: Status::Started,
    }
}

#[derive(PartialEq, Debug)]
enum Status {
    Started,
    HeaderOk,
    FirstCr,
    FirstLf,
    SecondCr,
    Done,
}

struct ConnectResponse {
    stream: Option<ProxyTcpStream>,
    status: Status,
}

impl Future for ConnectResponse {
    type Item = ProxyTcpStream;
    type Error = IoError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.stream.as_ref().map(|s| s.is_proxied) == Some(true) {
            let s = self.stream.as_mut().unwrap();

            if self.status == Status::Started {
                let mut status = [0; 12];
                try_nb!(s.read_exact(&mut status));
                // check status code of proxy response
                let status = match ::std::str::from_utf8(&status) {
                    Err(_) => return Err(other_error("Invalid status - not UTF8")),
                    Ok(s) => match str::parse::<u16>(&s[9..12]) {
                        Ok(n) => n,
                        Err(_) => return Err(other_error("Invalid status - not number")),
                    },
                };

                if status < 200 || status >= 300 {
                    return Err(other_error("Invalid status - not 2xx"));
                }

                self.status = Status::HeaderOk
            }

            loop {
                let mut next_byte = [0; 1];
                try_nb!(s.read_exact(&mut next_byte));

                match (&self.status, next_byte[0]) {
                    (&Status::HeaderOk, b'\r') => self.status = Status::FirstCr,
                    (&Status::FirstCr, b'\n') => self.status = Status::FirstLf,
                    (&Status::FirstCr, _) => return Err(other_error("Invalid end of line")),
                    (&Status::FirstLf, b'\r') => self.status = Status::SecondCr,
                    (&Status::FirstLf, _) => self.status = Status::HeaderOk,
                    (&Status::SecondCr, b'\n') => break,
                    (&Status::SecondCr, _) => return Err(other_error("Invalid end of line")),
                    (&Status::HeaderOk, _) => (),
                    (_, _) => {
                        return Err(other_error(&format!(
                            "Invalid Header - status {:?} on byte: {:?}",
                            &self.status,
                            next_byte[0]
                        )))
                    }
                }
            }
        }
        self.status = Status::Done;
        Ok(self.stream.take().unwrap().into())
    }
}

impl ProxyTcpStream {
    pub fn connect(addr: Tunnel, proxy: Option<&Proxy>, handle: Remote) -> IoFuture<Self> {
        let handle2 = handle.clone();
        let addr2 = addr.clone();
        let socket: Box<Future<Item=_, Error=IoError>+Send> = match proxy {
            None => {
                debug!(
                    "Connecting directly to {}:{}",
                    addr.remote_host,
                    addr.remote_port
                );
                Box::new(tcp_connect(&addr, handle).map(|s| (s,false)))
            }
            Some(p) => {
                debug!("Connecting via proxy {}:{}", p.host, p.port);
                Box::new(tcp_connect((&p.host[..], p.port), handle)
                .map(|s| (s, true))
                .or_else(move |e| {
                    warn!("Proxy connection failed {:?}, trying direct", e);
                    tcp_connect(&addr2, handle2).map(|s| (s,false))
                    
                })
                
                )
            }
        };
        
        let f = socket
            .map(move |(stream, prox) | {
                ProxyTcpStream {
                    inner: Arc::new(stream),
                    is_proxied: prox,
                }
            })
            .and_then(|stream| stream.write_proxy_connect(addr))
            .and_then(|stream| read_proxy_response(stream));
            

        Box::new(f)
    }

    fn write_proxy_connect(self, tun: Tunnel) -> IoFuture<Self> {
        let connect_string = if self.is_proxied {
            format!(
                "CONNECT {}:{} HTTP/1.1\r\n\r\n",
                &tun.remote_host,
                tun.remote_port
            )
        } else {
            "".to_owned()
        };
        let f =
            ::tokio_io::io::write_all(self, connect_string).and_then(|(socket, _req)| Ok(socket));

        Box::new(f)
    }
}

impl Debug for ProxyTcpStream {
    fn fmt(&self, fmt: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(fmt, "{:?}", self.inner)
    }
}

impl Read for ProxyTcpStream {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        (&*self.inner).read(buf)
    }
}

impl AsyncRead for ProxyTcpStream {}

impl Write for ProxyTcpStream {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        (&*self.inner).write(buf)
    }

    fn flush(&mut self) -> IoResult<()> {
        (&*self.inner).flush()
    }
}

impl AsyncWrite for ProxyTcpStream {
    fn shutdown(&mut self) -> Poll<(), IoError> {
        self.inner.shutdown(Shutdown::Write)?;
        Ok(().into())
    }
}

#[derive(Clone)]
pub struct FixedTcpStream(Arc<TcpStream>);

impl From<TcpStream> for FixedTcpStream {
    fn from(s: TcpStream) -> Self {
        FixedTcpStream(Arc::new(s))
    }
}

impl Read for FixedTcpStream {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        (&*self.0).read(buf)
    }
}

impl AsyncRead for FixedTcpStream {}

impl Write for FixedTcpStream {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        (&*self.0).write(buf)
    }

    fn flush(&mut self) -> IoResult<()> {
        (&*self.0).flush()
    }
}

impl AsyncWrite for FixedTcpStream {
    fn shutdown(&mut self) -> Poll<(), IoError> {
        self.0.shutdown(Shutdown::Write)?;
        Ok(().into())
    }
}

fn other_error(text: &str) -> IoError {
    IoError::new(IoErrorKind::Other, text)
}

#[cfg(test)]
mod tests {

    // #[test]
    // fn test_buf() {
    //     use tokio_io::io::{read_until, shutdown, read_exact};
    //     use tokio_core::net::{TcpStream};
    //     use tokio_core::reactor::Core;
    //     use std::net::{SocketAddr};
    //     use futures::Future;


    //     let r = Core::new().unwrap();
    //     let h = r.handle();
    //     let a = "127.0.0.1:80".parse().unwrap();
    //     let mut buf =vec![];
    //     let f=TcpStream::connect(&a, &h)
    //     .and_then(|s| {
    //         read_exact(s, buf)
    //         });

    // }
}