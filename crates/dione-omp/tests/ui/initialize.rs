async fn forbidden(control: dione_omp::OmpControl) {
    let _ = control.initialized().await;
}
fn main() {}
