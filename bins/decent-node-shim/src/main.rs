//! decent-node — DEPRECATED compatibility shim.
//!
//! Renamed to `decent` in the pre-v0.1 CLI migration. This shim prints a
//! deprecation warning and forwards all arguments to the `decent` binary.
//! Will be removed in v0.1.

fn main() {
    eprintln!("⚠ `decent-node` has been renamed to `decent`.");
    eprintln!("  Please use `decent` instead. This shim will be removed in v0.1.\n");

    // Forward to the `decent` binary. If it's not installed, tell the user.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cmd = std::process::Command::new("decent");
    cmd.args(&args);

    match cmd.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(_) => {
            eprintln!(
                "`decent` binary not found. Install it:\n  \
				 brew install decent-render/tap/decent\n  \
				 or: cargo install decent"
            );
            std::process::exit(1);
        }
    }
}
