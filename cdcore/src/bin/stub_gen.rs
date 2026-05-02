fn main() -> pyo3_stub_gen::Result<()> {
    let stub = cdcore::python::stub_info()?;
    stub.generate()?;
    Ok(())
}
