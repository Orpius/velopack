use anyhow::{anyhow, bail, Result};
use regex::Regex;
use semver::Version;
use std::{
    cell::RefCell,
    fs::{self, File},
    io::{Cursor, Read, Seek},
    path::{Path, PathBuf},
    rc::Rc,
};
use xml::reader::{EventReader, XmlEvent};
use zip::ZipArchive;

#[cfg(target_os = "windows")]
use chrono::{Datelike, Local as DateTime};

pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

#[derive(Clone)]
pub struct BundleInfo<'a> {
    zip: Rc<RefCell<ZipArchive<Box<dyn ReadSeek + 'a>>>>,
    zip_from_file: bool,
    zip_range: Option<&'a [u8]>,
    file_path: Option<PathBuf>,
}

pub fn load_bundle_from_file<'a>(file_name: &PathBuf) -> Result<BundleInfo<'a>> {
    debug!("Loading bundle from file '{}'...", file_name.display());
    let file = super::retry_io(|| File::open(&file_name))?;
    let cursor: Box<dyn ReadSeek> = Box::new(file);
    let zip = ZipArchive::new(cursor)?;
    return Ok(BundleInfo { zip: Rc::new(RefCell::new(zip)), zip_from_file: true, file_path: Some(file_name.to_owned()), zip_range: None });
}

impl BundleInfo<'_> {
    pub fn calculate_size(&self) -> (u64, u64) {
        let mut total_uncompressed_size = 0u64;
        let mut total_compressed_size = 0u64;
        let mut archive = self.zip.borrow_mut();

        for i in 0..archive.len() {
            let file = archive.by_index(i);
            if file.is_ok() {
                let file = file.unwrap();
                total_uncompressed_size += file.size();
                total_compressed_size += file.compressed_size();
            }
        }

        (total_compressed_size, total_uncompressed_size)
    }

    pub fn get_splash_bytes(&self) -> Option<Vec<u8>> {
        let splash_idx = self.find_zip_file(|name| name.contains("splashimage"));
        if splash_idx.is_none() {
            warn!("Could not find splash image in bundle.");
            return None;
        }

        let mut archive = self.zip.borrow_mut();
        let sf = archive.by_index(splash_idx.unwrap());
        if sf.is_err() {
            warn!("Could not find splash image in bundle.");
            return None;
        }

        let res: Result<Vec<u8>, _> = sf.unwrap().bytes().collect();
        if res.is_err() {
            warn!("Could not find splash image in bundle.");
            return None;
        }

        let bytes = res.unwrap();
        if bytes.is_empty() {
            warn!("Could not find splash image in bundle.");
            return None;
        }

        Some(bytes)
    }

    pub fn find_zip_file<F>(&self, predicate: F) -> Option<usize>
    where
        F: Fn(&str) -> bool,
    {
        let mut archive = self.zip.borrow_mut();
        for i in 0..archive.len() {
            if let Ok(file) = archive.by_index(i) {
                let name = file.name();
                if predicate(name) {
                    return Some(i);
                }
            }
        }
        None
    }

    pub fn extract_zip_idx_to_path<T: AsRef<Path>>(&self, index: usize, path: T) -> Result<()> {
        let path = path.as_ref();
        debug!("Extracting zip file to path: {}", path.to_string_lossy());
        let p = PathBuf::from(path);
        let parent = p.parent().unwrap();

        if !parent.exists() {
            debug!("Creating parent directory: {:?}", parent);
            super::retry_io(|| fs::create_dir_all(parent))?;
        }

        let mut archive = self.zip.borrow_mut();
        let mut file = archive.by_index(index)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        debug!("Writing file to disk: {:?}", path);
        super::retry_io(|| fs::write(path, &buffer))?;
        Ok(())
    }

    pub fn extract_zip_predicate_to_path<F, T: AsRef<Path>>(&self, predicate: F, path: T) -> Result<usize>
    where
        F: Fn(&str) -> bool,
    {
        let idx = self.find_zip_file(predicate);
        if idx.is_none() {
            bail!("Could not find file in bundle.");
        }
        let idx = idx.unwrap();
        self.extract_zip_idx_to_path(idx, path)?;
        Ok(idx)
    }

    pub fn extract_lib_contents_to_path<P: AsRef<Path>, F: Fn(i16)>(&self, current_path: P, progress: F) -> Result<()> {
        let current_path = current_path.as_ref();
        let files = self.get_file_names()?;
        let num_files = files.len();

        info!("Extracting {} app files to current directory...", num_files);
        let re = Regex::new(r"lib[\\\/][^\\\/]*[\\\/]").unwrap();
        let stub_regex = Regex::new("_ExecutionStub.exe$").unwrap();
        let updater_idx = self.find_zip_file(|name| name.ends_with("Squirrel.exe"));

        let nuspec_path = current_path.join("sq.version");
        let _ = self
            .extract_zip_predicate_to_path(|name| name.ends_with(".nuspec"), nuspec_path)
            .map_err(|_| anyhow!("This package is missing a nuspec manifest."))?;

        for (i, key) in files.iter().enumerate() {
            if Some(i) == updater_idx || !re.is_match(key) || key.ends_with("/") || key.ends_with("\\") {
                info!("    {} Skipped '{}'", i, key);
                continue;
            }

            let file_path_in_zip = re.replace(key, "").to_string();
            let file_path_on_disk = Path::new(&current_path).join(&file_path_in_zip);

            if stub_regex.is_match(&file_path_in_zip) {
                // let stub_key = stub_regex.replace(&file_path_in_zip, ".exe").to_string();
                // file_path_on_disk = root_path.join(&stub_key);
                info!("    {} Skipped Stub (obsolete) '{}'", i, key);
                continue;
            }

            let final_path = file_path_on_disk.to_str().unwrap().replace("/", "\\");
            info!("    {} Extracting '{}' to '{}'", i, key, final_path);

            self.extract_zip_idx_to_path(i, &final_path)?;
            progress(((i as f32 / num_files as f32) * 100.0) as i16);
        }

        Ok(())
    }

    pub fn read_manifest(&self) -> Result<Manifest> {
        let nuspec_idx = self
            .find_zip_file(|name| name.ends_with(".nuspec"))
            .ok_or_else(|| anyhow!("This installer is missing a package manifest (.nuspec). Please contact the application author."))?;
        let mut contents = String::new();
        let mut archive = self.zip.borrow_mut();
        archive.by_index(nuspec_idx)?.read_to_string(&mut contents)?;
        let app = read_manifest_from_string(&contents)?;
        Ok(app)
    }

    pub fn copy_bundle_to_file<T: AsRef<str>>(&self, nupkg_path: T) -> Result<()> {
        let nupkg_path = nupkg_path.as_ref();
        if self.zip_from_file {
            super::retry_io(|| fs::copy(self.file_path.clone().unwrap(), nupkg_path))?;
        } else {
            super::retry_io(|| fs::write(nupkg_path, self.zip_range.unwrap()))?;
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        let archive = self.zip.borrow();
        archive.len()
    }

    pub fn get_file_names(&self) -> Result<Vec<String>> {
        let mut files: Vec<String> = Vec::new();
        let mut archive = self.zip.borrow_mut();
        for i in 0..archive.len() {
            let file = archive.by_index(i)?;
            let key = file.name();
            files.push(key.to_string());
        }
        Ok(files)
    }
}

#[derive(Debug, derivative::Derivative, Clone)]
#[derivative(Default)]
pub struct Manifest {
    pub id: String,
    #[derivative(Default(value = "Version::new(0, 0, 0)"))]
    pub version: Version,
    pub title: String,
    pub authors: String,
    pub description: String,
    pub machine_architecture: String,
    pub runtime_dependencies: String,
    pub main_exe: String,
    pub os: String,
    pub os_min_version: String,
}

#[cfg(target_os = "windows")]
impl Manifest {
    const UNINST_STR: &'static str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";

    pub fn get_update_path(&self, root_path: &PathBuf) -> String {
        root_path.join("Update.exe").to_string_lossy().to_string()
    }
    pub fn get_main_exe_path(&self, root_path: &PathBuf) -> String {
        root_path.join("current").join(&self.main_exe).to_string_lossy().to_string()
    }
    pub fn get_packages_path(&self, root_path: &PathBuf) -> String {
        root_path.join("packages").to_string_lossy().to_string()
    }
    pub fn get_current_path(&self, root_path: &PathBuf) -> String {
        root_path.join("current").to_string_lossy().to_string()
    }
    pub fn get_nuspec_path(&self, root_path: &PathBuf) -> String {
        root_path.join("current").join("sq.version").to_string_lossy().to_string()
    }
    pub fn get_target_nupkg_path(&self, root_path: &PathBuf) -> String {
        root_path.join("packages").join(format!("{}-{}-full.nupkg", self.id, self.version)).to_string_lossy().to_string()
    }
    pub fn write_uninstall_entry(&self, root_path: &PathBuf) -> Result<()> {
        info!("Writing uninstall registry key...");
        let root_path_str = root_path.to_string_lossy().to_string();
        let main_exe_path = self.get_main_exe_path(root_path);
        let updater_path = self.get_update_path(root_path);

        let folder_size = fs_extra::dir::get_size(&root_path).unwrap();
        let sver = &self.version;
        let sver_str = format!("{}.{}.{}", sver.major, sver.minor, sver.patch);

        let now = DateTime::now();
        let formatted_date = format!("{}{:02}{:02}", now.year(), now.month(), now.day());

        let uninstall_cmd = format!("\"{}\" --uninstall", updater_path);
        let uninstall_quiet = format!("\"{}\" --uninstall --silent", updater_path);

        let reg_uninstall = w::HKEY::CURRENT_USER.RegCreateKeyEx(Self::UNINST_STR, None, co::REG_OPTION::NoValue, co::KEY::CREATE_SUB_KEY, None)?.0;
        let reg_app = reg_uninstall.RegCreateKeyEx(&self.id, None, co::REG_OPTION::NoValue, co::KEY::ALL_ACCESS, None)?.0;
        reg_app.RegSetKeyValue(None, Some("DisplayIcon"), w::RegistryValue::Sz(main_exe_path))?;
        reg_app.RegSetKeyValue(None, Some("DisplayName"), w::RegistryValue::Sz(self.title.to_owned()))?;
        reg_app.RegSetKeyValue(None, Some("DisplayVersion"), w::RegistryValue::Sz(sver_str))?;
        reg_app.RegSetKeyValue(None, Some("InstallDate"), w::RegistryValue::Sz(formatted_date))?;
        reg_app.RegSetKeyValue(None, Some("InstallLocation"), w::RegistryValue::Sz(root_path_str.to_owned()))?;
        reg_app.RegSetKeyValue(None, Some("Publisher"), w::RegistryValue::Sz(self.authors.to_owned()))?;
        reg_app.RegSetKeyValue(None, Some("QuietUninstallString"), w::RegistryValue::Sz(uninstall_quiet))?;
        reg_app.RegSetKeyValue(None, Some("UninstallString"), w::RegistryValue::Sz(uninstall_cmd))?;
        reg_app.RegSetKeyValue(None, Some("EstimatedSize"), w::RegistryValue::Dword((folder_size / 1024).try_into()?))?;
        reg_app.RegSetKeyValue(None, Some("NoModify"), w::RegistryValue::Dword(1))?;
        reg_app.RegSetKeyValue(None, Some("NoRepair"), w::RegistryValue::Dword(1))?;
        reg_app.RegSetKeyValue(None, Some("Language"), w::RegistryValue::Dword(0x0409))?;
        Ok(())
    }
    pub fn remove_uninstall_entry(&self) -> Result<()> {
        info!("Removing uninstall registry keys...");
        let reg_uninstall = w::HKEY::CURRENT_USER.RegCreateKeyEx(Self::UNINST_STR, None, co::REG_OPTION::NoValue, co::KEY::CREATE_SUB_KEY, None)?.0;
        reg_uninstall.RegDeleteKey(&self.id)?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Manifest {
    pub fn get_packages_path(&self, _root_path: &PathBuf) -> String {
        let tmp = format!("/tmp/clowd.squirrel/{}/packages", self.id);
        let p = Path::new(&tmp);
        if !p.exists() {
            fs::create_dir_all(p).unwrap();
        }
        p.to_string_lossy().to_string()
    }
    pub fn get_current_path(&self, root_path: &PathBuf) -> String {
        root_path.to_string_lossy().to_string()
    }
    pub fn get_nuspec_path(&self, root_path: &PathBuf) -> String {
        root_path.join("Contents").join("MacOS").join("sq.version").to_string_lossy().to_string()
    }
}

pub fn read_manifest_from_string(xml: &str) -> Result<Manifest> {
    let mut obj: Manifest = Default::default();
    let cursor = Cursor::new(xml);
    let parser = EventReader::new(cursor);
    let mut vec: Vec<String> = Vec::new();
    for e in parser {
        match e {
            Ok(XmlEvent::StartElement { name, .. }) => {
                vec.push(name.local_name);
            }
            Ok(XmlEvent::Characters(text)) => {
                if vec.is_empty() {
                    continue;
                }
                let el_name = vec.last().unwrap();
                if el_name == "id" {
                    obj.id = text;
                } else if el_name == "version" {
                    obj.version = Version::parse(&text)?;
                } else if el_name == "title" {
                    obj.title = text;
                } else if el_name == "authors" {
                    obj.authors = text;
                } else if el_name == "description" {
                    obj.description = text;
                } else if el_name == "machineArchitecture" {
                    obj.machine_architecture = text;
                } else if el_name == "runtimeDependencies" {
                    obj.runtime_dependencies = text;
                } else if el_name == "mainExe" {
                    obj.main_exe = text;
                } else if el_name == "os" {
                    obj.os = text;
                } else if el_name == "osMinVersion" {
                    obj.os_min_version = text;
                }
            }
            Ok(XmlEvent::EndElement { .. }) => {
                vec.pop();
            }
            Err(e) => {
                error!("Error: {e}");
                break;
            }
            // There's more: https://docs.rs/xml-rs/latest/xml/reader/enum.XmlEvent.html
            _ => {}
        }
    }

    if obj.id.is_empty() {
        bail!("Missing 'id' in package manifest. Please contact the application author.");
    }

    if !obj.os.is_empty() && obj.os != "win" {
        bail!("Unsupported 'os' in package manifest ({}). Please contact the application author.", obj.os);
    }

    if obj.version == Version::new(0, 0, 0) {
        bail!("Missing 'version' in package manifest. Please contact the application author.");
    }

    if obj.main_exe.is_empty() {
        bail!("Missing 'mainExe' in package manifest. Please contact the application author.");
    }

    if obj.title.is_empty() {
        obj.title = obj.id.clone();
    }

    Ok(obj)
}

#[derive(Debug, Clone, derivative::Derivative)]
#[derivative(Default)]
pub struct EntryNameInfo {
    pub name: String,
    #[derivative(Default(value = "Version::new(0, 0, 0)"))]
    pub version: Version,
    pub is_delta: bool,
    pub file_path: String,
    pub os: Option<String>,
    pub os_min_ver: Option<String>,
    pub os_arch: Option<String>,
}

impl EntryNameInfo {
    pub fn load_manifest(&self) -> Result<Manifest> {
        let path = Path::new(&self.file_path).to_path_buf();
        let bundle = load_bundle_from_file(&path)?;
        bundle.read_manifest()
    }
}

lazy_static! {
    static ref ENTRY_SUFFIX_FULL: Regex = Regex::new(r"(?i)-full.nupkg$").unwrap();
    static ref ENTRY_SUFFIX_DELTA: Regex = Regex::new(r"(?i)-delta.nupkg$").unwrap();
    static ref ENTRY_VERSION_START: Regex = Regex::new(r"[\.-](0|[1-9]\d*)\.(0|[1-9]\d*)($|[^\d])").unwrap();
    static ref ENTRY_RID: Regex = Regex::new(r"(?i)(-(?<os>osx|win)\.?(?<ver>[\d\.]+)?)?(?:-(?<arch>x64|x86|arm64))?$").unwrap();
}

pub fn parse_package_file_path(path: PathBuf) -> Option<EntryNameInfo> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let m = parse_package_file_name(name);
    if m.is_some() {
        let mut m = m.unwrap();
        m.file_path = path.to_string_lossy().to_string();
        return Some(m);
    }
    m
}

fn parse_package_file_name<T: AsRef<str>>(name: T) -> Option<EntryNameInfo> {
    let name = name.as_ref();
    let full = ENTRY_SUFFIX_FULL.is_match(name);
    let delta = ENTRY_SUFFIX_DELTA.is_match(name);
    if !full && !delta {
        return None;
    }

    let mut entry = EntryNameInfo::default();
    entry.is_delta = delta;

    let name_and_ver = if full { ENTRY_SUFFIX_FULL.replace(name, "") } else { ENTRY_SUFFIX_DELTA.replace(name, "") };
    let ver_idx = ENTRY_VERSION_START.find(&name_and_ver);
    if ver_idx.is_none() {
        return None;
    }

    let ver_idx = ver_idx.unwrap().start();
    entry.name = name_and_ver[0..ver_idx].to_string();
    let ver_idx = ver_idx + 1;
    let version = name_and_ver[ver_idx..].to_string();

    let rid_idx = ENTRY_RID.find(&version);
    if rid_idx.is_none() {
        let sv = Version::parse(&version);
        if sv.is_err() {
            return None;
        }

        entry.version = sv.unwrap();
        return Some(entry);
    }

    let rid_idx = rid_idx.unwrap().start();
    let caps = ENTRY_RID.captures(&version).unwrap();
    let version = version[0..rid_idx].to_string();

    let sv = Version::parse(&version);
    if sv.is_err() {
        return None;
    }

    entry.version = sv.unwrap();
    entry.os = caps.name("os").map(|m| m.as_str().to_string());
    entry.os_min_ver = caps.name("ver").map(|m| m.as_str().to_string());
    entry.os_arch = caps.name("arch").map(|m| m.as_str().to_string());

    Some(entry)
}

#[test]
fn test_parse_package_file_name() {
    // test no rid
    let entry = parse_package_file_name("Clowd.Squirrel-1.0.0-full.nupkg").unwrap();
    assert_eq!(entry.name, "Clowd.Squirrel");
    assert_eq!(entry.version, Version::parse("1.0.0").unwrap());
    assert_eq!(entry.is_delta, false);
    assert_eq!(entry.os, None);
    assert_eq!(entry.os_min_ver, None);
    assert_eq!(entry.os_arch, None);

    let entry = parse_package_file_name("Clowd.Squirrel-1.0.0-delta.nupkg").unwrap();
    assert_eq!(entry.name, "Clowd.Squirrel");
    assert_eq!(entry.version, Version::parse("1.0.0").unwrap());
    assert_eq!(entry.is_delta, true);
    assert_eq!(entry.os, None);
    assert_eq!(entry.os_min_ver, None);
    assert_eq!(entry.os_arch, None);

    let entry = parse_package_file_name("My.Cool-App-1.1.0-full.nupkg").unwrap();
    assert_eq!(entry.name, "My.Cool-App");
    assert_eq!(entry.version, Version::parse("1.1.0").unwrap());
    assert_eq!(entry.is_delta, false);
    assert_eq!(entry.os, None);
    assert_eq!(entry.os_min_ver, None);
    assert_eq!(entry.os_arch, None);

    // test with rid individual components
    let entry = parse_package_file_name("Clowd.Squirrel-1.0.0-osx-full.nupkg").unwrap();
    assert_eq!(entry.name, "Clowd.Squirrel");
    assert_eq!(entry.version, Version::parse("1.0.0").unwrap());
    assert_eq!(entry.is_delta, false);
    assert_eq!(entry.os, Some("osx".to_string()));
    assert_eq!(entry.os_min_ver, None);
    assert_eq!(entry.os_arch, None);

    let entry = parse_package_file_name("Clowd.Squirrel-1.0.0-win-full.nupkg").unwrap();
    assert_eq!(entry.name, "Clowd.Squirrel");
    assert_eq!(entry.version, Version::parse("1.0.0").unwrap());
    assert_eq!(entry.is_delta, false);
    assert_eq!(entry.os, Some("win".to_string()));
    assert_eq!(entry.os_min_ver, None);
    assert_eq!(entry.os_arch, None);

    let entry = parse_package_file_name("Clowd.Squirrel-1.0.0-x86-full.nupkg").unwrap();
    assert_eq!(entry.name, "Clowd.Squirrel");
    assert_eq!(entry.version, Version::parse("1.0.0").unwrap());
    assert_eq!(entry.is_delta, false);
    assert_eq!(entry.os, None);
    assert_eq!(entry.os_min_ver, None);
    assert_eq!(entry.os_arch, Some("x86".to_string()));

    let entry = parse_package_file_name("Clowd.Squirrel-1.0.0-x64-full.nupkg").unwrap();
    assert_eq!(entry.name, "Clowd.Squirrel");
    assert_eq!(entry.version, Version::parse("1.0.0").unwrap());
    assert_eq!(entry.is_delta, false);
    assert_eq!(entry.os, None);
    assert_eq!(entry.os_min_ver, None);
    assert_eq!(entry.os_arch, Some("x64".to_string()));

    let entry = parse_package_file_name("Clowd.Squirrel-1.0.0-arm64-full.nupkg").unwrap();
    assert_eq!(entry.name, "Clowd.Squirrel");
    assert_eq!(entry.version, Version::parse("1.0.0").unwrap());
    assert_eq!(entry.is_delta, false);
    assert_eq!(entry.os, None);
    assert_eq!(entry.os_min_ver, None);
    assert_eq!(entry.os_arch, Some("arm64".to_string()));

    // test with full rid
    let entry = parse_package_file_name("Clowd.Squirrel-1.0.0-win10-x64-full.nupkg").unwrap();
    assert_eq!(entry.name, "Clowd.Squirrel");
    assert_eq!(entry.version, Version::parse("1.0.0").unwrap());
    assert_eq!(entry.is_delta, false);
    assert_eq!(entry.os, Some("win".to_string()));
    assert_eq!(entry.os_min_ver, Some("10".to_string()));
    assert_eq!(entry.os_arch, Some("x64".to_string()));

    let entry = parse_package_file_name("Clowd.Squirrel-1.0.0-win10-arm64-full.nupkg").unwrap();
    assert_eq!(entry.name, "Clowd.Squirrel");
    assert_eq!(entry.version, Version::parse("1.0.0").unwrap());
    assert_eq!(entry.is_delta, false);
    assert_eq!(entry.os, Some("win".to_string()));
    assert_eq!(entry.os_min_ver, Some("10".to_string()));
    assert_eq!(entry.os_arch, Some("arm64".to_string()));

    // test with version extras
    let entry = parse_package_file_name("MyCoolApp-1.2.3-beta1-win7-x64-full.nupkg").unwrap();
    assert_eq!(entry.name, "MyCoolApp");
    assert_eq!(entry.version, Version::parse("1.2.3-beta1").unwrap());
    assert_eq!(entry.is_delta, false);
    assert_eq!(entry.os, Some("win".to_string()));
    assert_eq!(entry.os_min_ver, Some("7".to_string()));
    assert_eq!(entry.os_arch, Some("x64".to_string()));

    let entry = parse_package_file_name("MyCoolApp-1.2.3-beta1-win7-x64-delta.nupkg").unwrap();
    assert_eq!(entry.name, "MyCoolApp");
    assert_eq!(entry.version, Version::parse("1.2.3-beta1").unwrap());
    assert_eq!(entry.is_delta, true);
    assert_eq!(entry.os, Some("win".to_string()));
    assert_eq!(entry.os_min_ver, Some("7".to_string()));
    assert_eq!(entry.os_arch, Some("x64".to_string()));

    let entry = parse_package_file_name("MyCoolApp-1.2.3-beta.22.44-win7-x64-full.nupkg").unwrap();
    assert_eq!(entry.name, "MyCoolApp");
    assert_eq!(entry.version, Version::parse("1.2.3-beta.22.44").unwrap());
    assert_eq!(entry.is_delta, false);
    assert_eq!(entry.os, Some("win".to_string()));
    assert_eq!(entry.os_min_ver, Some("7".to_string()));
    assert_eq!(entry.os_arch, Some("x64".to_string()));

    // test invalid names
    assert!(parse_package_file_name("MyCoolApp-1.2.3-beta1-win7-x64-full.nupkg.zip").is_none());
    assert!(parse_package_file_name("MyCoolApp-1.2.3-beta1-win7-x64-full.zip").is_none());
    assert!(parse_package_file_name("MyCoolApp-1.2.3.nupkg").is_none());
    assert!(parse_package_file_name("MyCoolApp-1.2-full.nupkg").is_none());
}
