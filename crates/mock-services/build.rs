fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(
            &[
                "../../proto/internal/v1/content.proto",
                "../../proto/internal/v1/profile.proto",
            ],
            &["../../proto"],
        )?;
    Ok(())
}
