// Smoke-test the update backend without the GUI:
//   cargo run --example check            # read cached state (no polkit prompt)
//   cargo run --example check -- refresh # refresh metadata first (polkit prompt)

#[tokio::main]
async fn main() {
    let refresh = std::env::args().any(|a| a == "refresh");
    println!("Checking for updates (refresh={refresh})…\n");

    let report = cosmic_applet_updates::backend::check_for_updates(refresh).await;

    println!("System updates: {}", report.system.len());
    for u in &report.system {
        println!("  - {} {}", u.name, u.version);
    }
    println!("\nFlatpak updates: {}", report.flatpak.len());
    for u in &report.flatpak {
        println!("  - {} {}", u.name, u.version);
    }
    if !report.errors.is_empty() {
        println!("\nErrors:");
        for e in &report.errors {
            println!("  ! {e}");
        }
    }
    println!("\nTotal: {}", report.total());
}
