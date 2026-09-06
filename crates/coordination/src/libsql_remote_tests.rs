use super::{guarded_connector, remote_connector};
use crate::Error;
use hyper::{Uri, service::Service};
use std::{
    future::{Ready, ready},
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

#[derive(Clone)]
struct CountingConnector {
    calls: Arc<AtomicUsize>,
}

impl Service<Uri> for CountingConnector {
    type Response = ();
    type Error = io::Error;
    type Future = Ready<std::result::Result<(), io::Error>>;

    fn poll_ready(
        &mut self,
        _context: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: Uri) -> Self::Future {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ready(Ok(()))
    }
}

#[tokio::test]
async fn https_filter_rejects_http_before_delegating() -> std::result::Result<(), io::Error> {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut connector = guarded_connector(CountingConnector {
        calls: Arc::clone(&calls),
    });
    let uri = "http://database.example/"
        .parse::<Uri>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid HTTP test URI"))?;
    let error = match connector.call(uri).await {
        Ok(()) => {
            return Err(io::Error::other(
                "HTTP test URI unexpectedly reached connector",
            ));
        }
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "remote connector requires HTTPS");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn https_filter_delegates_https_and_selected_connector_keeps_guard()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut connector = guarded_connector(CountingConnector {
        calls: Arc::clone(&calls),
    });
    connector
        .call("https://database.example/".parse::<Uri>()?)
        .await?;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let mut selected = match remote_connector() {
        Ok(connector) => connector,
        Err(Error::Backend("load native TLS roots")) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let Err(error) = selected
        .call("http://database.example/".parse::<Uri>()?)
        .await
    else {
        return Err("selected connector accepted HTTP".into());
    };
    assert_eq!(error.to_string(), "remote connector requires HTTPS");
    Ok(())
}
