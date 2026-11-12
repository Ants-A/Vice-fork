//! Print the native video environment without starting GTK or the recorder.
//! --nvidia exercises the compatibility policy on a machine with another GPU.
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let nvidia = args.iter().any(|arg| arg == "--nvidia") || vice::native_video::nvidia_gpu();
    let hardware = !args.iter().any(|arg| arg == "--software");
    let ranks = std::env::var("GST_PLUGIN_FEATURE_RANK").unwrap_or_default();
    let policy: std::collections::BTreeMap<_, _> =
        vice::native_video::environment(hardware, nvidia, &ranks)
            .into_iter()
            .chain(
                vice::native_video::download_environment()
                    .into_iter()
                    .map(|(key, value)| (key, value.to_owned())),
            )
            .collect();
    println!("{}", serde_json::to_string(&policy)?);
    Ok(())
}
