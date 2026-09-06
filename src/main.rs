slint::include_modules!();

fn main() {
    let window =
        slint_generatedMainWindow::MainWindow::new().expect("failed to create MainWindow");
    window.run().expect("failed to run slint window");
}
