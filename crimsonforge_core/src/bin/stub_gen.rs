fn main() -> pyo3_stub_gen::Result<()> {
    let stub = crimsonforge_core::python::stub_info()?;
    stub.generate()?;
    Ok(())
}
