use super::error::{Error, Result};
use libloading::{Library, Symbol};
use once_cell::unsync::OnceCell;
use regex::{Regex, RegexBuilder};
use serde_derive::Deserialize;
use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use std::{fs, mem};
use tree_sitter::Language;
use tree_sitter_highlight::{HighlightConfiguration, Highlighter};

#[cfg(unix)]
const DYLIB_EXTENSION: &'static str = "so";

#[cfg(windows)]
const DYLIB_EXTENSION: &'static str = "dll";

const BUILD_TARGET: &'static str = env!("BUILD_TARGET");

#[derive(Default)]
pub struct LanguageConfiguration {
    pub scope: Option<String>,
    pub content_regex: Option<Regex>,
    pub _first_line_regex: Option<Regex>,
    pub injection_regex: Option<Regex>,
    pub file_types: Vec<String>,
    pub root_path: PathBuf,
    pub highlights_filenames: Option<Vec<String>>,
    pub injections_filenames: Option<Vec<String>>,
    pub locals_filenames: Option<Vec<String>>,
    language_id: usize,
    highlight_config: OnceCell<Option<HighlightConfiguration>>,
}

pub struct Loader {
    parser_lib_path: PathBuf,
    languages_by_id: Vec<(PathBuf, OnceCell<Language>)>,
    language_configurations: Vec<LanguageConfiguration>,
    language_configuration_ids_by_file_type: HashMap<String, Vec<usize>>,
}

unsafe impl Send for Loader {}
unsafe impl Sync for Loader {}

impl Loader {
    pub fn new(parser_lib_path: PathBuf) -> Self {
        Loader {
            parser_lib_path,
            languages_by_id: Vec::new(),
            language_configurations: Vec::new(),
            language_configuration_ids_by_file_type: HashMap::new(),
        }
    }

    pub fn find_all_languages(&mut self, parser_src_paths: &Vec<PathBuf>) -> Result<()> {
        for parser_container_dir in parser_src_paths.iter() {
            if let Ok(entries) = fs::read_dir(parser_container_dir) {
                for entry in entries {
                    let entry = entry?;
                    if let Some(parser_dir_name) = entry.file_name().to_str() {
                        if parser_dir_name.starts_with("tree-sitter-") {
                            self.find_language_configurations_at_path(
                                &parser_container_dir.join(parser_dir_name),
                            )
                            .ok();
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn languages_at_path(&mut self, path: &Path) -> Result<Vec<Language>> {
        if let Ok(configurations) = self.find_language_configurations_at_path(path) {
            let mut language_ids = configurations
                .iter()
                .map(|c| c.language_id)
                .collect::<Vec<_>>();
            language_ids.sort();
            language_ids.dedup();
            language_ids
                .into_iter()
                .map(|id| self.language_for_id(id))
                .collect::<Result<Vec<_>>>()
        } else {
            Ok(Vec::new())
        }
    }

    pub fn get_all_language_configurations(&self) -> Vec<(&LanguageConfiguration, &Path)> {
        self.language_configurations
            .iter()
            .map(|c| (c, self.languages_by_id[c.language_id].0.as_ref()))
            .collect()
    }

    pub fn language_configuration_for_scope(
        &self,
        scope: &str,
    ) -> Result<Option<(Language, &LanguageConfiguration)>> {
        for configuration in &self.language_configurations {
            if configuration.scope.as_ref().map_or(false, |s| s == scope) {
                let language = self.language_for_id(configuration.language_id)?;
                return Ok(Some((language, configuration)));
            }
        }
        Ok(None)
    }

    pub fn language_configuration_for_file_name(
        &self,
        path: &Path,
    ) -> Result<Option<(Language, &LanguageConfiguration)>> {
        // Find all the language configurations that match this file name
        // or a suffix of the file name.
        let configuration_ids = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|file_name| self.language_configuration_ids_by_file_type.get(file_name))
            .or_else(|| {
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .and_then(|extension| {
                        self.language_configuration_ids_by_file_type.get(extension)
                    })
            });

        if let Some(configuration_ids) = configuration_ids {
            if !configuration_ids.is_empty() {
                let configuration;

                // If there is only one language configuration, then use it.
                if configuration_ids.len() == 1 {
                    configuration = &self.language_configurations[configuration_ids[0]];
                }
                // If multiple language configurations match, then determine which
                // one to use by applying the configurations' content regexes.
                else {
                    let file_contents = fs::read_to_string(path)?;
                    let mut best_score = -2isize;
                    let mut best_configuration_id = None;
                    for configuration_id in configuration_ids {
                        let config = &self.language_configurations[*configuration_id];

                        // If the language configuration has a content regex, assign
                        // a score based on the length of the first match.
                        let score;
                        if let Some(content_regex) = &config.content_regex {
                            if let Some(mat) = content_regex.find(&file_contents) {
                                score = (mat.end() - mat.start()) as isize;
                            }
                            // If the content regex does not match, then *penalize* this
                            // language configuration, so that language configurations
                            // without content regexes are preferred over those with
                            // non-matching content regexes.
                            else {
                                score = -1;
                            }
                        } else {
                            score = 0;
                        }
                        if score > best_score {
                            best_configuration_id = Some(*configuration_id);
                            best_score = score;
                        }
                    }

                    configuration = &self.language_configurations[best_configuration_id.unwrap()];
                }

                let language = self.language_for_id(configuration.language_id)?;
                return Ok(Some((language, configuration)));
            }
        }

        Ok(None)
    }

    pub fn language_configuration_for_injection_string(
        &self,
        string: &str,
    ) -> Result<Option<(Language, &LanguageConfiguration)>> {
        let mut best_match_length = 0;
        let mut best_match_position = None;
        for (i, configuration) in self.language_configurations.iter().enumerate() {
            if let Some(injection_regex) = &configuration.injection_regex {
                if let Some(mat) = injection_regex.find(string) {
                    let length = mat.end() - mat.start();
                    if length > best_match_length {
                        best_match_position = Some(i);
                        best_match_length = length;
                    }
                }
            }
        }

        if let Some(i) = best_match_position {
            let configuration = &self.language_configurations[i];
            let language = self.language_for_id(configuration.language_id)?;
            Ok(Some((language, configuration)))
        } else {
            Ok(None)
        }
    }

    fn language_for_id(&self, id: usize) -> Result<Language> {
        let (path, language) = &self.languages_by_id[id];
        language
            .get_or_try_init(|| {
                let src_path = path.join("src");
                self.load_language_at_path(&src_path, &src_path)
            })
            .map(|l| *l)
    }

    pub fn load_language_at_path(&self, src_path: &Path, header_path: &Path) -> Result<Language> {
        let grammar_path = src_path.join("grammar.json");
        let parser_path = src_path.join("parser.c");
        let mut scanner_path = src_path.join("scanner.c");

        #[derive(Deserialize)]
        struct GrammarJSON {
            name: String,
        }
        let mut grammar_file =
            fs::File::open(grammar_path).map_err(Error::wrap(|| "Failed to read grammar.json"))?;
        let grammar_json: GrammarJSON = serde_json::from_reader(BufReader::new(&mut grammar_file))
            .map_err(Error::wrap(|| "Failed to parse grammar.json"))?;

        let scanner_path = if scanner_path.exists() {
            Some(scanner_path)
        } else {
            scanner_path.set_extension("cc");
            if scanner_path.exists() {
                Some(scanner_path)
            } else {
                None
            }
        };

        self.load_language_from_sources(
            &grammar_json.name,
            &header_path,
            &parser_path,
            &scanner_path,
        )
    }

    pub fn load_language_from_sources(
        &self,
        name: &str,
        header_path: &Path,
        parser_path: &Path,
        scanner_path: &Option<PathBuf>,
    ) -> Result<Language> {
        let mut library_path = self.parser_lib_path.join(name);
        library_path.set_extension(DYLIB_EXTENSION);

        let recompile = needs_recompile(&library_path, &parser_path, &scanner_path).map_err(
            Error::wrap(|| "Failed to compare source and binary timestamps"),
        )?;

        if recompile {
            let mut config = cc::Build::new();
            config
                .cpp(true)
                .opt_level(2)
                .cargo_metadata(false)
                .target(BUILD_TARGET)
                .host(BUILD_TARGET);
            let compiler = config.get_compiler();
            let mut command = Command::new(compiler.path());
            for (key, value) in compiler.env() {
                command.env(key, value);
            }

            if cfg!(windows) {
                command
                    .args(&["/nologo", "/LD", "/I"])
                    .arg(header_path)
                    .arg("/Od")
                    .arg(parser_path);
                if let Some(scanner_path) = scanner_path.as_ref() {
                    command.arg(scanner_path);
                }
                command
                    .arg("/link")
                    .arg(format!("/out:{}", library_path.to_str().unwrap()));
            } else {
                command
                    .arg("-shared")
                    .arg("-fPIC")
                    .arg("-fno-exceptions")
                    .arg("-g")
                    .arg("-I")
                    .arg(header_path)
                    .arg("-o")
                    .arg(&library_path)
                    .arg("-O2");
                if let Some(scanner_path) = scanner_path.as_ref() {
                    if scanner_path.extension() == Some("c".as_ref()) {
                        command.arg("-xc").arg("-std=c99").arg(scanner_path);
                    } else {
                        command.arg(scanner_path);
                    }
                }
                command.arg("-xc").arg(parser_path);
            }

            let output = command
                .output()
                .map_err(Error::wrap(|| "Failed to execute C compiler"))?;
            if !output.status.success() {
                return Err(Error::new(format!(
                    "Parser compilation failed.\nStdout: {}\nStderr: {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
        }

        let library = Library::new(&library_path).map_err(Error::wrap(|| {
            format!("Error opening dynamic library {:?}", &library_path)
        }))?;
        let language_fn_name = format!("tree_sitter_{}", replace_dashes_with_underscores(name));
        let language = unsafe {
            let language_fn: Symbol<unsafe extern "C" fn() -> Language> = library
                .get(language_fn_name.as_bytes())
                .map_err(Error::wrap(|| {
                    format!("Failed to load symbol {}", language_fn_name)
                }))?;
            language_fn()
        };
        mem::forget(library);
        Ok(language)
    }

    fn find_language_configurations_at_path<'a>(
        &'a mut self,
        parser_path: &Path,
    ) -> Result<&[LanguageConfiguration]> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum PathsJSON {
            Empty,
            Single(String),
            Multiple(Vec<String>),
        }

        impl Default for PathsJSON {
            fn default() -> Self {
                PathsJSON::Empty
            }
        }

        impl PathsJSON {
            fn into_vec(self) -> Option<Vec<String>> {
                match self {
                    PathsJSON::Empty => None,
                    PathsJSON::Single(s) => Some(vec![s]),
                    PathsJSON::Multiple(s) => Some(s),
                }
            }
        }

        #[derive(Deserialize)]
        struct LanguageConfigurationJSON {
            #[serde(default)]
            path: PathBuf,
            scope: Option<String>,
            #[serde(rename = "file-types")]
            file_types: Option<Vec<String>>,
            #[serde(rename = "content-regex")]
            content_regex: Option<String>,
            #[serde(rename = "first-line-regex")]
            first_line_regex: Option<String>,
            #[serde(rename = "injection-regex")]
            injection_regex: Option<String>,
            #[serde(default)]
            highlights: PathsJSON,
            #[serde(default)]
            injections: PathsJSON,
            #[serde(default)]
            locals: PathsJSON,
        }

        #[derive(Deserialize)]
        struct PackageJSON {
            #[serde(default)]
            #[serde(rename = "tree-sitter")]
            tree_sitter: Vec<LanguageConfigurationJSON>,
        }

        let initial_language_configuration_count = self.language_configurations.len();

        if let Ok(package_json_contents) = fs::read_to_string(&parser_path.join("package.json")) {
            let package_json = serde_json::from_str::<PackageJSON>(&package_json_contents);
            if let Ok(package_json) = package_json {
                let language_count = self.languages_by_id.len();
                for config_json in package_json.tree_sitter {
                    // Determine the path to the parser directory. This can be specified in
                    // the package.json, but defaults to the directory containing the package.json.
                    let language_path = parser_path.join(config_json.path);

                    // Determine if a previous language configuration in this package.json file
                    // already uses the same language.
                    let mut language_id = None;
                    for (id, (path, _)) in
                        self.languages_by_id.iter().enumerate().skip(language_count)
                    {
                        if language_path == *path {
                            language_id = Some(id);
                        }
                    }

                    // If not, add a new language path to the list.
                    let language_id = language_id.unwrap_or_else(|| {
                        self.languages_by_id.push((language_path, OnceCell::new()));
                        self.languages_by_id.len() - 1
                    });

                    let configuration = LanguageConfiguration {
                        root_path: parser_path.to_path_buf(),
                        scope: config_json.scope,
                        language_id,
                        file_types: config_json.file_types.unwrap_or(Vec::new()),
                        content_regex: config_json
                            .content_regex
                            .and_then(|r| RegexBuilder::new(&r).multi_line(true).build().ok()),
                        _first_line_regex: config_json
                            .first_line_regex
                            .and_then(|r| RegexBuilder::new(&r).multi_line(true).build().ok()),
                        injection_regex: config_json
                            .injection_regex
                            .and_then(|r| RegexBuilder::new(&r).multi_line(true).build().ok()),
                        highlight_config: OnceCell::new(),
                        injections_filenames: config_json.injections.into_vec(),
                        locals_filenames: config_json.locals.into_vec(),
                        highlights_filenames: config_json.highlights.into_vec(),
                    };

                    for file_type in &configuration.file_types {
                        self.language_configuration_ids_by_file_type
                            .entry(file_type.to_string())
                            .or_insert(Vec::new())
                            .push(self.language_configurations.len());
                    }

                    self.language_configurations.push(configuration);
                }
            }
        }

        if self.language_configurations.len() == initial_language_configuration_count
            && parser_path.join("src").join("grammar.json").exists()
        {
            let mut configuration = LanguageConfiguration::default();
            configuration.root_path = parser_path.to_owned();
            configuration.language_id = self.languages_by_id.len();
            self.language_configurations.push(configuration);
            self.languages_by_id
                .push((parser_path.to_owned(), OnceCell::new()));
        }

        Ok(&self.language_configurations[initial_language_configuration_count..])
    }
}

impl LanguageConfiguration {
    pub fn highlight_config(
        &self,
        highlighter: &Highlighter,
        language: Language,
    ) -> Result<Option<&HighlightConfiguration>> {
        self.highlight_config
            .get_or_try_init(|| {
                let queries_path = self.root_path.join("queries");
                let read_queries = |paths: &Option<Vec<String>>, default_path: &str| {
                    if let Some(paths) = paths.as_ref() {
                        let mut query = String::new();
                        for path in paths {
                            let path = self.root_path.join(path);
                            query += &fs::read_to_string(&path).map_err(Error::wrap(|| {
                                format!("Failed to read query file {:?}", path)
                            }))?;
                        }
                        Ok(query)
                    } else {
                        let path = queries_path.join(default_path);
                        if path.exists() {
                            fs::read_to_string(&path).map_err(Error::wrap(|| {
                                format!("Failed to read query file {:?}", path)
                            }))
                        } else {
                            Ok(String::new())
                        }
                    }
                };

                let highlights_query = read_queries(&self.highlights_filenames, "highlights.scm")?;
                let injections_query = read_queries(&self.injections_filenames, "injections.scm")?;
                let locals_query = read_queries(&self.locals_filenames, "locals.scm")?;

                if highlights_query.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(
                        highlighter
                            .load_configuration(
                                language,
                                &highlights_query,
                                &injections_query,
                                &locals_query,
                            )
                            .map_err(Error::wrap(|| {
                                format!("Failed to load queries in {:?}", self.root_path)
                            }))?,
                    ))
                }
            })
            .map(Option::as_ref)
    }
}

fn needs_recompile(
    lib_path: &Path,
    parser_c_path: &Path,
    scanner_path: &Option<PathBuf>,
) -> Result<bool> {
    if !lib_path.exists() {
        return Ok(true);
    }
    let lib_mtime = mtime(lib_path)?;
    if mtime(parser_c_path)? > lib_mtime {
        return Ok(true);
    }
    if let Some(scanner_path) = scanner_path {
        if mtime(scanner_path)? > lib_mtime {
            return Ok(true);
        }
    }
    Ok(false)
}

fn mtime(path: &Path) -> Result<SystemTime> {
    Ok(fs::metadata(path)?.modified()?)
}

fn replace_dashes_with_underscores(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for c in name.chars() {
        if c == '-' {
            result.push('_');
        } else {
            result.push(c);
        }
    }
    result
}
