use super::*;
use std::net::Ipv6Addr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[test]
fn rejects_empty_proxy_list() {
    let err = Socks5Connector::new(Vec::<String>::new(), ("target.example", 4242)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn adds_default_port() {
    assert_eq!(with_default_port("proxy.example", 1080), "proxy.example:1080");
    assert_eq!(with_default_port("proxy.example:4242", 1080), "proxy.example:4242");
    assert_eq!(with_default_port("127.0.0.1", 1080), "127.0.0.1:1080");
    assert_eq!(with_default_port("127.0.0.1:4242", 1080), "127.0.0.1:4242");
    assert_eq!(with_default_port("::1", 1080), "[::1]:1080");
    assert_eq!(with_default_port("[::1]:4242", 1080), "[::1]:4242");
}

#[test]
fn parses_target() {
    assert_eq!(
        Socks5Target::parse("target.example", 4242).unwrap(),
        Socks5Target::Domain("target.example".into(), 4242)
    );
    assert_eq!(
        Socks5Target::parse("target.example:1234", 4242).unwrap(),
        Socks5Target::Domain("target.example".into(), 1234)
    );
    assert_eq!(
        Socks5Target::parse("127.0.0.1", 4242).unwrap(),
        Socks5Target::Ip(SocketAddr::from(([127, 0, 0, 1], 4242)))
    );
    assert_eq!(
        Socks5Target::parse("[::1]:1234", 4242).unwrap(),
        Socks5Target::Ip(SocketAddr::from((Ipv6Addr::LOCALHOST, 1234)))
    );
}

#[tokio::test]
async fn publishes_one_link_for_each_proxy() {
    let proxies = [SocketAddr::from(([127, 0, 0, 1], 1080)), SocketAddr::from(([127, 0, 0, 2], 1080))];
    let connector =
        Socks5Connector::new(proxies.map(|proxy| proxy.to_string()), ("target.example", 4242)).unwrap();
    let (tx, mut rx) = watch::channel(HashSet::new());
    let task = tokio::spawn(async move { connector.link_tags(tx).await });

    rx.changed().await.unwrap();
    let tags = rx.borrow().clone();
    assert_eq!(tags.len(), proxies.len());
    for proxy in proxies {
        assert!(tags.contains(
            &(Box::new(Socks5LinkTag::new(proxy, Socks5Target::Domain("target.example".into(), 4242),))
                as LinkTagBox)
        ));
    }

    task.abort();
}

#[tokio::test]
async fn connects_through_proxy() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();

        let mut greeting = [0; 2];
        socket.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting[0], 5);
        let mut methods = vec![0; greeting[1] as usize];
        socket.read_exact(&mut methods).await.unwrap();
        assert!(methods.contains(&0));
        socket.write_all(&[5, 0]).await.unwrap();

        let mut request = [0; 4];
        socket.read_exact(&mut request).await.unwrap();
        assert_eq!(&request[..3], &[5, 1, 0]);
        assert_eq!(request[3], 3);
        let domain_len = socket.read_u8().await.unwrap() as usize;
        let mut domain = vec![0; domain_len];
        socket.read_exact(&mut domain).await.unwrap();
        assert_eq!(domain, b"target.example");
        assert_eq!(socket.read_u16().await.unwrap(), 4242);

        socket.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await.unwrap();
        let mut request = [0; 4];
        socket.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"ping");
        socket.write_all(b"pong").await.unwrap();
    });

    let target = Socks5Target::parse("target.example", 4242).unwrap();
    let connector = Socks5Connector::new([proxy.to_string()], target.clone()).unwrap();
    let tag = Socks5LinkTag::new(proxy, target);
    let stream = connector.connect(&tag).await.unwrap();
    let StreamBox::Io(mut stream) = stream else { panic!("expected IO stream") };
    stream.write_all(b"ping").await.unwrap();
    let mut response = [0; 4];
    stream.read_exact(&mut response).await.unwrap();
    assert_eq!(&response, b"pong");

    server.await.unwrap();
}
