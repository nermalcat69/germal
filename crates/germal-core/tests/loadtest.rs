use germal_core::http::{OutboundBody, build_client};
use germal_core::{http::HttpRequest, loadtest, model::Method};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn sends_exactly_total_requests() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(20)
        .mount(&server)
        .await;
    let req = HttpRequest {
        method: Method::Get,
        url: server.uri().parse().unwrap(),
        headers: vec![],
        body: OutboundBody::Empty,
    };
    let r = loadtest::run(&build_client(), &req, 20, 4).await;
    assert_eq!((r.total, r.completed, r.errors), (20, 20, 0));
    assert_eq!(r.statuses[&200], 20);
}
