fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[
                "../../proto/social/v1/common.proto",
                "../../proto/social/v1/likes.proto",
                "../../proto/social/v1/health.proto",
                "../../proto/internal/v1/content.proto",
                "../../proto/internal/v1/profile.proto",
            ],
            &["../../proto"],
        )?;
    Ok(())
}
