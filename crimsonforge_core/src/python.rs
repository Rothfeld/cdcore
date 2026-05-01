//! PyO3 Python extension module.
//!
//! Exposes the full Rust API as a native Python extension.
//! Import as: `import crimsonforge_core as cf`

use pyo3::prelude::*;
use pyo3_stub_gen::derive::{gen_stub_pyclass, gen_stub_pymethods, gen_stub_pyfunction};
use pyo3::exceptions::{PyIOError, PyValueError};
use pyo3::types::{PyBytes, PyList, PyDict};

use crate::{crypto, compression};
use crate::archive::pamt;
use crate::formats::{mesh, animation, physics, scene, data};
use crate::vfs::VfsManager as RustVfsManager;
use crate::repack::engine::{RepackEngine as RustRepackEngine, ModifiedFile as RustModifiedFile};
use crate::error::ParseError;

fn to_pyerr(e: ParseError) -> PyErr {
    match e {
        ParseError::Io(io) => PyIOError::new_err(io.to_string()),
        other => PyValueError::new_err(other.to_string()),
    }
}

// ── PamtFileEntry ─────────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "PamtFileEntry", skip_from_py_object)]
#[derive(Clone)]
pub struct PyPamtFileEntry {
    inner: pamt::PamtFileEntry,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyPamtFileEntry {
    #[getter] fn path(&self) -> &str      { &self.inner.path }
    #[getter] fn paz_file(&self) -> &str  { &self.inner.paz_file }
    #[getter] fn offset(&self) -> u64     { self.inner.offset }
    #[getter] fn comp_size(&self) -> u32  { self.inner.comp_size }
    #[getter] fn orig_size(&self) -> u32  { self.inner.orig_size }
    #[getter] fn flags(&self) -> u32      { self.inner.flags }
    #[getter] fn paz_index(&self) -> u8   { self.inner.paz_index }
    #[getter] fn compressed(&self) -> bool     { self.inner.compressed() }
    #[getter] fn compression_type(&self) -> u8 { self.inner.compression_type() }
    #[getter] fn encrypted(&self) -> bool      { self.inner.encrypted() }
    #[getter] fn record_offset(&self) -> usize { self.inner.record_offset }

    fn __repr__(&self) -> String {
        format!("PamtFileEntry(path={:?}, comp_size={}, orig_size={}, encrypted={})",
            self.inner.path, self.inner.comp_size, self.inner.orig_size, self.inner.encrypted())
    }
}

// ── PamtData ──────────────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "PamtData", skip_from_py_object)]
#[derive(Clone)]
pub struct PyPamtData {
    inner: pamt::PamtData,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyPamtData {
    #[getter] fn path(&self) -> &str           { &self.inner.path }
    #[getter] fn self_crc(&self) -> u32        { self.inner.self_crc }
    #[getter] fn paz_count(&self) -> u32       { self.inner.paz_count }
    #[getter] fn folder_prefix(&self) -> &str  { &self.inner.folder_prefix }

    #[getter]
    fn file_entries(&self) -> Vec<PyPamtFileEntry> {
        self.inner.file_entries.iter().map(|e| PyPamtFileEntry { inner: e.clone() }).collect()
    }

    fn find_entry(&self, filename: &str) -> Option<PyPamtFileEntry> {
        let needle = filename.replace('\\', "/").to_lowercase();
        let needle_base = needle.rsplit('/').next().unwrap_or(&needle);
        let mut fallback = None;
        for e in &self.inner.file_entries {
            let epath = e.path.replace('\\', "/").to_lowercase();
            if epath == needle {
                return Some(PyPamtFileEntry { inner: e.clone() });
            }
            if fallback.is_none() {
                let ebase = epath.rsplit('/').next().unwrap_or(&epath);
                if ebase == needle_base {
                    fallback = Some(PyPamtFileEntry { inner: e.clone() });
                }
            }
        }
        fallback
    }

    fn __repr__(&self) -> String {
        format!("PamtData(path={:?}, {} entries)", self.inner.path, self.inner.file_entries.len())
    }
}

// ── VfsManager ────────────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "VfsManager")]
pub struct PyVfsManager {
    inner: RustVfsManager,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyVfsManager {
    #[new]
    fn new(packages_path: &str) -> PyResult<Self> {
        let inner = RustVfsManager::new(packages_path).map_err(to_pyerr)?;
        Ok(PyVfsManager { inner })
    }

    fn load_all_groups(&self) -> PyResult<()> {
        self.inner.load_all_groups().map_err(to_pyerr)
    }

    fn load_group(&self, group_dir: &str) -> PyResult<()> {
        self.inner.load_group(group_dir).map_err(to_pyerr)
    }

    fn list_groups(&self) -> PyResult<Vec<String>> {
        self.inner.list_groups().map_err(to_pyerr)
    }

    fn lookup(&self, path: &str) -> Option<PyPamtFileEntry> {
        self.inner.lookup(path).map(|e| PyPamtFileEntry { inner: e })
    }

    fn read_entry(&self, py: Python<'_>, entry: &PyPamtFileEntry) -> PyResult<Py<PyBytes>> {
        let data = self.inner.read_entry(&entry.inner).map_err(to_pyerr)?;
        Ok(PyBytes::new(py, &data).unbind())
    }

    fn search(&self, query: &str) -> Vec<PyPamtFileEntry> {
        self.inner.search(query)
            .into_iter()
            .map(|e| PyPamtFileEntry { inner: e })
            .collect()
    }

    fn list_dir(&self, path: &str) -> Vec<String> {
        self.inner.list_dir(path)
    }

    fn reload(&mut self) -> PyResult<()> {
        self.inner.reload().map_err(to_pyerr)
    }

    fn invalidate_group(&self, group_dir: &str) {
        self.inner.invalidate_group(group_dir);
    }

    fn get_pamt(&self, group_dir: &str) -> Option<PyPamtData> {
        self.inner.get_pamt(group_dir).map(|d| PyPamtData { inner: d })
    }

    #[getter]
    fn packages_path(&self) -> &str {
        self.inner.packages_path()
    }
}

// ── SubMesh ───────────────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "SubMesh", skip_from_py_object)]
#[derive(Clone)]
pub struct PySubMesh {
    inner: mesh::SubMesh,
}

#[gen_stub_pymethods]
#[pymethods]
impl PySubMesh {
    #[getter] fn name(&self)     -> &str  { &self.inner.name }
    #[getter] fn material(&self) -> &str  { &self.inner.material }
    #[getter] fn texture(&self)  -> &str  { &self.inner.texture }
    #[getter] fn vertex_count(&self) -> usize { self.inner.vertex_count }
    #[getter] fn face_count(&self)   -> usize { self.inner.face_count }

    #[getter]
    fn vertices(&self) -> Vec<(f32, f32, f32)> {
        self.inner.vertices.iter().map(|v| (v[0], v[1], v[2])).collect()
    }
    #[getter]
    fn uvs(&self) -> Vec<(f32, f32)> {
        self.inner.uvs.iter().map(|uv| (uv[0], uv[1])).collect()
    }
    #[getter]
    fn normals(&self) -> Vec<(f32, f32, f32)> {
        self.inner.normals.iter().map(|n| (n[0], n[1], n[2])).collect()
    }
    #[getter]
    fn faces(&self) -> Vec<(u32, u32, u32)> {
        self.inner.faces.iter().map(|f| (f[0], f[1], f[2])).collect()
    }
}

// ── ParsedMesh ────────────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "ParsedMesh")]
pub struct PyParsedMesh {
    inner: mesh::ParsedMesh,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyParsedMesh {
    #[getter] fn path(&self)    -> &str  { &self.inner.path }
    #[getter] fn format(&self)  -> &str  { &self.inner.format }
    #[getter] fn has_uvs(&self) -> bool  { self.inner.has_uvs }
    #[getter] fn total_vertices(&self) -> usize { self.inner.total_vertices }
    #[getter] fn total_faces(&self)    -> usize { self.inner.total_faces }
    #[getter] fn bbox_min(&self) -> (f32, f32, f32) { let b = self.inner.bbox_min; (b[0],b[1],b[2]) }
    #[getter] fn bbox_max(&self) -> (f32, f32, f32) { let b = self.inner.bbox_max; (b[0],b[1],b[2]) }

    #[getter]
    fn submeshes(&self) -> Vec<PySubMesh> {
        self.inner.submeshes.iter().map(|sm| PySubMesh { inner: sm.clone() }).collect()
    }
}

// ── ParsedPac ─────────────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "ParsedPac")]
pub struct PyParsedPac {
    inner: mesh::ParsedPac,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyParsedPac {
    #[getter] fn path(&self) -> &str { &self.inner.path }
    #[getter] fn has_uvs(&self)   -> bool  { self.inner.has_uvs }
    #[getter] fn has_bones(&self) -> bool  { self.inner.has_bones }
    #[getter] fn total_vertices(&self) -> usize { self.inner.total_vertices }
    #[getter] fn total_faces(&self)    -> usize { self.inner.total_faces }
    #[getter] fn bbox_min(&self) -> (f32, f32, f32) { let b = self.inner.bbox_min; (b[0],b[1],b[2]) }
    #[getter] fn bbox_max(&self) -> (f32, f32, f32) { let b = self.inner.bbox_max; (b[0],b[1],b[2]) }

    #[getter]
    fn submeshes(&self) -> Vec<PySubMesh> {
        self.inner.submeshes.iter().map(|psm| PySubMesh { inner: psm.base.clone() }).collect()
    }
}

// ── Skeleton / Bone ───────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "Bone", skip_from_py_object)]
#[derive(Clone)]
pub struct PyBone {
    inner: animation::Bone,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyBone {
    #[getter] fn index(&self)        -> usize { self.inner.index }
    #[getter] fn name(&self)         -> &str  { &self.inner.name }
    #[getter] fn parent_index(&self) -> i32   { self.inner.parent_index }
    #[getter] fn scale(&self)    -> (f32,f32,f32) { let s=self.inner.scale; (s[0],s[1],s[2]) }
    #[getter] fn rotation(&self) -> (f32,f32,f32,f32) { let r=self.inner.rotation; (r[0],r[1],r[2],r[3]) }
    #[getter] fn position(&self) -> (f32,f32,f32) { let p=self.inner.position; (p[0],p[1],p[2]) }

    fn __repr__(&self) -> String {
        format!("Bone(index={}, name={:?}, parent={})", self.inner.index, self.inner.name, self.inner.parent_index)
    }
}

#[gen_stub_pyclass]
#[pyclass(name = "Skeleton")]
pub struct PySkeleton {
    inner: animation::Skeleton,
}

#[gen_stub_pymethods]
#[pymethods]
impl PySkeleton {
    #[getter] fn path(&self) -> &str { &self.inner.path }

    #[getter]
    fn bones(&self) -> Vec<PyBone> {
        self.inner.bones.iter().map(|b| PyBone { inner: b.clone() }).collect()
    }

    #[getter]
    fn bone_count(&self) -> usize { self.inner.bones.len() }
}

// ── ParsedAnimation ───────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "Keyframe", skip_from_py_object)]
#[derive(Clone)]
pub struct PyKeyframe {
    pub rotation:    (f32, f32, f32, f32),
    pub translation: (f32, f32, f32),
    pub scale:       (f32, f32, f32),
}

#[gen_stub_pymethods]
#[pymethods]
impl PyKeyframe {
    #[getter] fn rotation(&self)    -> (f32,f32,f32,f32) { self.rotation }
    #[getter] fn translation(&self) -> (f32,f32,f32)     { self.translation }
    #[getter] fn scale(&self)       -> (f32,f32,f32)     { self.scale }
}

#[gen_stub_pyclass]
#[pyclass(name = "ParsedAnimation")]
pub struct PyParsedAnimation {
    inner: animation::ParsedAnimation,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyParsedAnimation {
    #[getter] fn path(&self)          -> &str  { &self.inner.path }
    #[getter] fn frame_count(&self)   -> u32   { self.inner.frame_count }
    #[getter] fn bone_count(&self)    -> u32   { self.inner.bone_count }
    #[getter] fn fps(&self)           -> f32   { self.inner.fps }
    #[getter] fn metadata_tags(&self) -> &str  { &self.inner.metadata_tags }
    #[getter] fn is_character(&self)  -> bool  { self.inner.variant == animation::AnimVariant::Character }

    /// keyframes[frame_idx][bone_idx] -> Keyframe
    fn get_keyframe(&self, py: Python<'_>, frame: usize, bone: usize) -> PyResult<Py<PyAny>> {
        let kf = self.inner.keyframes.get(frame)
            .and_then(|f| f.get(bone))
            .ok_or_else(|| PyValueError::new_err("frame/bone index out of range"))?;
        let py_kf = PyKeyframe {
            rotation:    (kf.rotation[0], kf.rotation[1], kf.rotation[2], kf.rotation[3]),
            translation: (kf.translation[0], kf.translation[1], kf.translation[2]),
            scale:       (kf.scale[0], kf.scale[1], kf.scale[2]),
        };
        Ok(py_kf.into_pyobject(py).unwrap().into_any().unbind())
    }
}

// ── PaaMetabin ────────────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "PaaMetabin")]
pub struct PyPaaMetabin {
    inner: animation::PaaMetabin,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyPaaMetabin {
    #[getter] fn path(&self)        -> &str  { &self.inner.path }
    #[getter] fn record_count(&self)-> usize { self.inner.records.len() }
    #[getter] fn file_size(&self)   -> usize { self.inner.file_size }

    fn records_as_list(&self, py: Python<'_>) -> Py<PyAny> {
        let list = PyList::empty(py);
        for r in &self.inner.records {
            let d = PyDict::new(py);
            let _ = d.set_item("subtype", r.subtype);
            let _ = d.set_item("tag", r.tag);
            let _ = d.set_item("offset", r.offset);
            let _ = d.set_item("payload", PyBytes::new(py, &r.payload));
            let _ = list.append(d);
        }
        list.into_any().unbind()
    }
}

// ── ParsedHavok ───────────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "ParsedHavok")]
pub struct PyParsedHavok {
    inner: physics::ParsedHavok,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyParsedHavok {
    #[getter] fn path(&self)        -> &str  { &self.inner.path }
    #[getter] fn sdk_version(&self) -> &str  { &self.inner.sdk_version }
    #[getter] fn has_skeleton(&self)  -> bool { self.inner.has_skeleton }
    #[getter] fn has_animation(&self) -> bool { self.inner.has_animation }
    #[getter] fn has_physics(&self)   -> bool { self.inner.has_physics }
    #[getter] fn has_ragdoll(&self)   -> bool { self.inner.has_ragdoll }
    #[getter] fn has_cloth(&self)     -> bool { self.inner.has_cloth }
    #[getter] fn has_mesh_shape(&self)-> bool { self.inner.has_mesh_shape }
    #[getter] fn binds_to_mesh_topology(&self) -> bool { self.inner.binds_to_mesh_topology }
    #[getter] fn class_names(&self) -> Vec<String> { self.inner.class_names.clone() }
    #[getter] fn shape_types(&self)  -> Vec<String> { self.inner.shape_types.clone() }
}

// ── ParsedNavmesh ─────────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "ParsedNavmesh")]
pub struct PyParsedNavmesh {
    inner: physics::ParsedNavmesh,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyParsedNavmesh {
    #[getter] fn path(&self)       -> &str  { &self.inner.path }
    #[getter] fn cell_count(&self) -> usize { self.inner.cell_count }
    #[getter] fn file_size(&self)  -> usize { self.inner.file_size }

    fn cells_as_list(&self, py: Python<'_>) -> Py<PyAny> {
        let list = PyList::empty(py);
        for c in &self.inner.cells {
            let d = PyDict::new(py);
            let _ = d.set_item("cell_id",  c.cell_id);
            let _ = d.set_item("grid_ref", c.grid_ref);
            let _ = d.set_item("flags",    c.flags);
            let _ = d.set_item("neighbor", c.neighbor);
            let _ = d.set_item("tile_x",   c.tile_x);
            let _ = list.append(d);
        }
        list.into_any().unbind()
    }
}

// ── ParsedPrefab ──────────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "ParsedPrefab")]
pub struct PyParsedPrefab {
    inner: scene::ParsedPrefab,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyParsedPrefab {
    #[getter] fn path(&self)             -> &str { &self.inner.path }
    #[getter] fn file_hash_1(&self)      -> u32  { self.inner.file_hash_1 }
    #[getter] fn file_hash_2(&self)      -> u32  { self.inner.file_hash_2 }
    #[getter] fn component_count(&self)  -> u32  { self.inner.component_count }

    fn strings_as_list(&self, py: Python<'_>) -> Py<PyAny> {
        let list = PyList::empty(py);
        for s in &self.inner.strings {
            let d = PyDict::new(py);
            let _ = d.set_item("prefix_offset", s.prefix_offset);
            let _ = d.set_item("value_offset",  s.value_offset);
            let _ = d.set_item("length",         s.length);
            let _ = d.set_item("value",          &s.value);
            let kind = match s.kind {
                scene::PrefabStringKind::FileRef      => "file_ref",
                scene::PrefabStringKind::EnumTag      => "enum_tag",
                scene::PrefabStringKind::PropertyName => "property_name",
                scene::PrefabStringKind::Unknown      => "unknown",
            };
            let _ = d.set_item("kind", kind);
            let _ = list.append(d);
        }
        list.into_any().unbind()
    }

    fn file_refs(&self) -> Vec<String> {
        self.inner.strings.iter()
            .filter(|s| s.kind == scene::PrefabStringKind::FileRef)
            .map(|s| s.value.clone())
            .collect()
    }
}

// ── PalocData ─────────────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "PalocData")]
pub struct PyPalocData {
    inner: data::PalocData,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyPalocData {
    #[getter] fn path(&self) -> &str { &self.inner.path }
    #[getter] fn entry_count(&self) -> usize { self.inner.entries.len() }

    fn entries_as_list(&self, py: Python<'_>) -> Py<PyAny> {
        let list = PyList::empty(py);
        for e in &self.inner.entries {
            let d = PyDict::new(py);
            let _ = d.set_item("key",          &e.key);
            let _ = d.set_item("value",        &e.value);
            let _ = d.set_item("key_offset",   e.key_offset);
            let _ = d.set_item("value_offset", e.value_offset);
            let _ = list.append(d);
        }
        list.into_any().unbind()
    }

    fn as_dict(&self, py: Python<'_>) -> Py<PyAny> {
        let d = PyDict::new(py);
        for e in &self.inner.entries {
            let _ = d.set_item(&e.key, &e.value);
        }
        d.into_any().unbind()
    }

    fn lookup(&self, key: &str) -> Option<String> {
        self.inner.entries.iter()
            .find(|e| e.key == key)
            .map(|e| e.value.clone())
    }
}

// ── PabgbTable ────────────────────────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "PabgbTable")]
pub struct PyPabgbTable {
    inner: data::PabgbTable,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyPabgbTable {
    #[getter] fn file_name(&self)  -> &str  { &self.inner.file_name }
    #[getter] fn row_count(&self)  -> usize { self.inner.rows.len() }
    #[getter] fn is_simple(&self)  -> bool  { self.inner.is_simple }
    #[getter] fn row_size(&self)   -> usize { self.inner.row_size }

    fn rows_as_list(&self, py: Python<'_>) -> Py<PyAny> {
        let list = PyList::empty(py);
        for row in &self.inner.rows {
            let d = PyDict::new(py);
            let _ = d.set_item("index",       row.index);
            let _ = d.set_item("row_hash",    row.row_hash);
            let _ = d.set_item("data_offset", row.data_offset);
            let _ = d.set_item("name",        &row.name);
            let fields = PyList::empty(py);
            for f in &row.fields {
                let fd = PyDict::new(py);
                let _ = fd.set_item("offset", f.offset);
                let _ = fd.set_item("size",   f.size);
                let disp = f.value.display();
                let _ = fd.set_item("value", disp);
                let kind = match &f.value {
                    data::FieldValue::U32(_) => "u32",
                    data::FieldValue::I32(_) => "i32",
                    data::FieldValue::F32(_) => "f32",
                    data::FieldValue::Str(_) => "str",
                    data::FieldValue::Blob(_) => "blob",
                };
                let _ = fd.set_item("kind", kind);
                let _ = fields.append(fd);
            }
            let _ = d.set_item("fields", fields);
            let _ = list.append(d);
        }
        list.into_any().unbind()
    }

    fn get_row_by_hash(&self, py: Python<'_>, hash: u32) -> Option<Py<PyAny>> {
        let row = self.inner.rows.iter().find(|r| r.row_hash == hash)?;
        let d = PyDict::new(py);
        let _ = d.set_item("index",    row.index);
        let _ = d.set_item("row_hash", row.row_hash);
        let _ = d.set_item("name",     &row.name);
        Some(d.into_any().unbind())
    }
}

// ── ModifiedFile / RepackEngine ───────────────────────────────────────────────

#[gen_stub_pyclass]
#[pyclass(name = "ModifiedFile")]
pub struct PyModifiedFile {
    data: Vec<u8>,
    entry: pamt::PamtFileEntry,
    pamt_data: pamt::PamtData,
    package_group: String,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyModifiedFile {
    #[new]
    fn new(
        data: Vec<u8>,
        entry: &PyPamtFileEntry,
        pamt_data: &PyPamtData,
        package_group: &str,
    ) -> Self {
        PyModifiedFile {
            data: data.to_vec(),
            entry: entry.inner.clone(),
            pamt_data: pamt_data.inner.clone(),
            package_group: package_group.to_string(),
        }
    }
}

#[gen_stub_pyclass]
#[pyclass(name = "RepackResult")]
pub struct PyRepackResult {
    pub success: bool,
    pub files_repacked: usize,
    pub paz_crc: u32,
    pub pamt_crc: u32,
    pub papgt_crc: u32,
    pub backup_dir: String,
    pub errors: Vec<String>,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyRepackResult {
    #[getter] fn success(&self)        -> bool   { self.success }
    #[getter] fn files_repacked(&self) -> usize  { self.files_repacked }
    #[getter] fn paz_crc(&self)        -> u32    { self.paz_crc }
    #[getter] fn pamt_crc(&self)       -> u32    { self.pamt_crc }
    #[getter] fn papgt_crc(&self)      -> u32    { self.papgt_crc }
    #[getter] fn backup_dir(&self)     -> &str   { &self.backup_dir }
    #[getter] fn errors(&self)         -> Vec<String> { self.errors.clone() }
}

#[gen_stub_pyclass]
#[pyclass(name = "RepackEngine")]
pub struct PyRepackEngine {
    inner: RustRepackEngine,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyRepackEngine {
    #[new]
    #[pyo3(signature = (packages_path, backup_dir=None))]
    fn new(packages_path: &str, backup_dir: Option<&str>) -> Self {
        PyRepackEngine { inner: RustRepackEngine::new(packages_path, backup_dir) }
    }

    #[pyo3(signature = (modified_files, papgt_path, create_backup=true))]
    fn repack(
        &self,
        modified_files: Vec<PyRef<PyModifiedFile>>,
        papgt_path: &str,
        create_backup: bool,
    ) -> PyResult<PyRepackResult> {
        let files: Vec<RustModifiedFile> = modified_files.iter().map(|mf| {
            RustModifiedFile {
                data: mf.data.clone(),
                entry: mf.entry.clone(),
                pamt_data: mf.pamt_data.clone(),
                package_group: mf.package_group.clone(),
            }
        }).collect();

        let result = self.inner.repack(files, papgt_path, create_backup).map_err(to_pyerr)?;

        Ok(PyRepackResult {
            success: result.success,
            files_repacked: result.files_repacked,
            paz_crc: result.paz_crc,
            pamt_crc: result.pamt_crc,
            papgt_crc: result.papgt_crc,
            backup_dir: result.backup_dir,
            errors: result.errors,
        })
    }
}

// ── Standalone functions ───────────────────────────────────────────────────────

#[gen_stub_pyfunction]
#[pyfunction]
fn pa_checksum(data: Vec<u8>) -> u32 {
    crypto::pa_checksum(&data)
}

#[gen_stub_pyfunction]
#[pyfunction]
fn decrypt(py: Python<'_>, data: Vec<u8>, filename: &str) -> Py<PyBytes> {
    let result = crypto::decrypt(&data, filename);
    PyBytes::new(py, &result).unbind()
}

#[gen_stub_pyfunction]
#[pyfunction]
fn encrypt(py: Python<'_>, data: Vec<u8>, filename: &str) -> Py<PyBytes> {
    let result = crypto::encrypt(&data, filename);
    PyBytes::new(py, &result).unbind()
}

#[gen_stub_pyfunction]
#[pyfunction]
fn is_encrypted(path: &str) -> bool {
    crypto::is_encrypted(path)
}

#[gen_stub_pyfunction]
#[pyfunction]
fn lz4_compress(py: Python<'_>, data: Vec<u8>) -> Py<PyBytes> {
    let result = compression::lz4::compress(&data);
    PyBytes::new(py, &result).unbind()
}

#[gen_stub_pyfunction]
#[pyfunction]
fn lz4_decompress(py: Python<'_>, data: Vec<u8>, orig_size: usize) -> PyResult<Py<PyBytes>> {
    let result = compression::lz4::decompress(&data, orig_size).map_err(to_pyerr)?;
    Ok(PyBytes::new(py, &result).unbind())
}

#[gen_stub_pyfunction]
#[pyfunction]
fn zlib_compress(py: Python<'_>, data: Vec<u8>) -> PyResult<Py<PyBytes>> {
    let result = compression::zlib::compress(&data).map_err(to_pyerr)?;
    Ok(PyBytes::new(py, &result).unbind())
}

#[gen_stub_pyfunction]
#[pyfunction]
fn zlib_decompress(py: Python<'_>, data: Vec<u8>) -> PyResult<Py<PyBytes>> {
    let result = compression::zlib::decompress(&data).map_err(to_pyerr)?;
    Ok(PyBytes::new(py, &result).unbind())
}

#[gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (data, orig_size, compression_type))]
fn decompress(py: Python<'_>, data: Vec<u8>, orig_size: usize, compression_type: u8) -> PyResult<Py<PyBytes>> {
    let result = compression::decompress(&data, orig_size, compression_type).map_err(to_pyerr)?;
    Ok(PyBytes::new(py, &result).unbind())
}

#[gen_stub_pyfunction]
#[pyfunction]
fn parse_pamt(pamt_path: &str, paz_dir: Option<&str>) -> PyResult<PyPamtData> {
    let inner = pamt::parse_pamt(pamt_path, paz_dir).map_err(to_pyerr)?;
    Ok(PyPamtData { inner })
}

#[gen_stub_pyfunction]
#[pyfunction]
fn parse_pam(_py: Python<'_>, data: Vec<u8>, filename: Option<&str>) -> PyResult<PyParsedMesh> {
    let name = filename.unwrap_or("");
    let inner = mesh::parse_pam(&data, name).map_err(to_pyerr)?;
    Ok(PyParsedMesh { inner })
}

#[gen_stub_pyfunction]
#[pyfunction]
fn parse_pamlod(data: Vec<u8>, filename: Option<&str>) -> PyResult<PyParsedMesh> {
    let name = filename.unwrap_or("");
    let inner = mesh::parse_pamlod(&data, name).map_err(to_pyerr)?;
    Ok(PyParsedMesh { inner })
}

#[gen_stub_pyfunction]
#[pyfunction]
fn parse_pac(data: Vec<u8>, filename: Option<&str>) -> PyResult<PyParsedPac> {
    let name = filename.unwrap_or("");
    let inner = mesh::parse_pac(&data, name).map_err(to_pyerr)?;
    Ok(PyParsedPac { inner })
}

#[gen_stub_pyfunction]
#[pyfunction]
fn parse_paa(data: Vec<u8>, filename: Option<&str>) -> PyResult<PyParsedAnimation> {
    let name = filename.unwrap_or("");
    let inner = animation::parse_paa(&data, name).map_err(to_pyerr)?;
    Ok(PyParsedAnimation { inner })
}

#[gen_stub_pyfunction]
#[pyfunction]
fn parse_paa_metabin(data: Vec<u8>, filename: Option<&str>) -> PyResult<PyPaaMetabin> {
    let name = filename.unwrap_or("");
    let inner = animation::parse_paa_metabin(&data, name).map_err(to_pyerr)?;
    Ok(PyPaaMetabin { inner })
}

#[gen_stub_pyfunction]
#[pyfunction]
fn parse_pab(data: Vec<u8>, filename: Option<&str>) -> PyResult<PySkeleton> {
    let name = filename.unwrap_or("");
    let inner = animation::parse_pab(&data, name).map_err(to_pyerr)?;
    Ok(PySkeleton { inner })
}

#[gen_stub_pyfunction]
#[pyfunction]
fn parse_hkx(data: Vec<u8>, filename: Option<&str>) -> PyResult<PyParsedHavok> {
    let name = filename.unwrap_or("");
    let inner = physics::parse_hkx(&data, name).map_err(to_pyerr)?;
    Ok(PyParsedHavok { inner })
}

#[gen_stub_pyfunction]
#[pyfunction]
fn parse_nav(data: Vec<u8>, filename: Option<&str>) -> PyResult<PyParsedNavmesh> {
    let name = filename.unwrap_or("");
    let inner = physics::parse_nav(&data, name).map_err(to_pyerr)?;
    Ok(PyParsedNavmesh { inner })
}

#[gen_stub_pyfunction]
#[pyfunction]
fn parse_prefab(data: Vec<u8>, filename: Option<&str>) -> PyResult<PyParsedPrefab> {
    let name = filename.unwrap_or("");
    let inner = scene::parse_prefab(&data, name).map_err(to_pyerr)?;
    Ok(PyParsedPrefab { inner })
}

#[gen_stub_pyfunction]
#[pyfunction]
fn parse_paloc(data: Vec<u8>, filename: Option<&str>) -> PyResult<PyPalocData> {
    let name = filename.unwrap_or("");
    let inner = data::parse_paloc(&data, name).map_err(to_pyerr)?;
    Ok(PyPalocData { inner })
}

#[gen_stub_pyfunction]
#[pyfunction]
fn parse_pabgb(
    pabgh_data: Vec<u8>,
    pabgb_data: Vec<u8>,
    filename: Option<&str>,
) -> PyResult<PyPabgbTable> {
    let name = filename.unwrap_or("");
    let inner = data::parse_pabgb(&pabgh_data, &pabgb_data, name).map_err(to_pyerr)?;
    Ok(PyPabgbTable { inner })
}

#[gen_stub_pyfunction]
#[pyfunction]
fn verify_chain(pamt_path: &str, papgt_path: &str) -> PyResult<bool> {
    crate::repack::verify_chain(pamt_path, papgt_path).map_err(to_pyerr)
}

#[gen_stub_pyfunction]
#[pyfunction]
fn decode_dds_to_rgba(py: Python<'_>, data: Vec<u8>) -> PyResult<(u32, u32, Py<PyBytes>)> {
    let (w, h, rgba) = crate::formats::dds::decode_dds_to_rgba(&data).map_err(to_pyerr)?;
    Ok((w, h, PyBytes::new(py, &rgba).unbind()))
}

pyo3_stub_gen::define_stub_info_gatherer!(stub_info);

// ── Module definition ─────────────────────────────────────────────────────────

#[pymodule]
pub fn crimsonforge_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Classes
    m.add_class::<PyPamtFileEntry>()?;
    m.add_class::<PyPamtData>()?;
    m.add_class::<PyVfsManager>()?;
    m.add_class::<PySubMesh>()?;
    m.add_class::<PyParsedMesh>()?;
    m.add_class::<PyParsedPac>()?;
    m.add_class::<PyBone>()?;
    m.add_class::<PySkeleton>()?;
    m.add_class::<PyKeyframe>()?;
    m.add_class::<PyParsedAnimation>()?;
    m.add_class::<PyPaaMetabin>()?;
    m.add_class::<PyParsedHavok>()?;
    m.add_class::<PyParsedNavmesh>()?;
    m.add_class::<PyParsedPrefab>()?;
    m.add_class::<PyPalocData>()?;
    m.add_class::<PyPabgbTable>()?;
    m.add_class::<PyModifiedFile>()?;
    m.add_class::<PyRepackResult>()?;
    m.add_class::<PyRepackEngine>()?;

    // Crypto
    m.add_function(wrap_pyfunction!(pa_checksum, m)?)?;
    m.add_function(wrap_pyfunction!(decrypt, m)?)?;
    m.add_function(wrap_pyfunction!(encrypt, m)?)?;
    m.add_function(wrap_pyfunction!(is_encrypted, m)?)?;

    // Compression
    m.add_function(wrap_pyfunction!(lz4_compress, m)?)?;
    m.add_function(wrap_pyfunction!(lz4_decompress, m)?)?;
    m.add_function(wrap_pyfunction!(zlib_compress, m)?)?;
    m.add_function(wrap_pyfunction!(zlib_decompress, m)?)?;
    m.add_function(wrap_pyfunction!(decompress, m)?)?;

    // Parsers
    m.add_function(wrap_pyfunction!(parse_pamt, m)?)?;
    m.add_function(wrap_pyfunction!(parse_pam, m)?)?;
    m.add_function(wrap_pyfunction!(parse_pamlod, m)?)?;
    m.add_function(wrap_pyfunction!(parse_pac, m)?)?;
    m.add_function(wrap_pyfunction!(parse_paa, m)?)?;
    m.add_function(wrap_pyfunction!(parse_paa_metabin, m)?)?;
    m.add_function(wrap_pyfunction!(parse_pab, m)?)?;
    m.add_function(wrap_pyfunction!(parse_hkx, m)?)?;
    m.add_function(wrap_pyfunction!(parse_nav, m)?)?;
    m.add_function(wrap_pyfunction!(parse_prefab, m)?)?;
    m.add_function(wrap_pyfunction!(parse_paloc, m)?)?;
    m.add_function(wrap_pyfunction!(parse_pabgb, m)?)?;

    // DDS
    m.add_function(wrap_pyfunction!(decode_dds_to_rgba, m)?)?;

    // Repack
    m.add_function(wrap_pyfunction!(verify_chain, m)?)?;

    // Version
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;

    Ok(())
}
