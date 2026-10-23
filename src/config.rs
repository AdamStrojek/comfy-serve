use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::fs;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FieldInputTarget {
    Text,
    ImageBase64,
    ImageUrl,
    ComfyUpload,
    AudioUpload,
}

impl Default for FieldInputTarget {
    fn default() -> Self {
        Self::Text
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkflowFieldMap {
    pub original_node_id: String,
    pub original_field_name: String,
    pub exposed_as: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub input_target: FieldInputTarget,
    #[serde(default)]
    pub randomize: bool,
    #[serde(default)]
    pub is_value_map: bool,
    #[serde(default)]
    pub map_keys: String,
    #[serde(default)]
    pub map_values: String,
}

/// Describes how optional input images are wired into a workflow: for every image a
/// `LoadImage` node is added and linked to `<field_prefix><N>` of node `node_id`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ImageInputsConfig {
    pub node_id: String,
    pub field_prefix: String,
    #[serde(default)]
    pub max_images: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkflowConfig {
    pub active: bool,
    pub file_name: String,
    pub exposed_fields: Vec<WorkflowFieldMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_inputs: Option<ImageInputsConfig>,
}

/// A video workflow in `active-workflows/video/<name>.json`, served by the asynchronous video API.
/// Everything but `active` defaults to the MiniMax H3 conventions, so `active = true` is enough
/// for a workflow built the same way.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct VideoWorkflowConfig {
    pub active: bool,

    /// Generator variant for text and first/last frame to video
    pub i2v_node: String,
    /// Generator variant for reference images/audio/videos to video
    pub r2v_node: String,
    /// Id the chosen variant gets; the rest of the graph links to it
    pub generator_node: String,
    /// Fields of the generator node
    pub prompt_field: String,
    pub width_field: String,
    pub height_field: String,
    pub length_field: String,
    pub first_frame_field: String,
    pub last_frame_field: String,
    /// Prefixes of the generator's dynamic reference inputs, followed by the loader node's id
    pub image_prefix: String,
    pub audio_prefix: String,
    pub video_prefix: String,
    pub seed_node: String,
    pub seed_field: String,
    /// Node whose `denoise` is set to 1, if it is in the workflow; empty to skip
    pub denoise_node: String,
    /// Save node whose `filename_prefix` is set
    pub output_node: String,

    pub min_duration: u64,
    pub max_duration: u64,
    pub default_duration: u64,
    pub fps: u64,
    /// The model takes `frame_step * k + frame_offset` frames; the count is rounded up to that
    pub frame_step: u64,
    pub frame_offset: u64,
    /// Width and height are rounded to multiples of this
    pub size_multiple: u64,
    /// Supported resolutions, each the length of the shorter side (e.g. "480p" = 480)
    pub resolutions: Vec<String>,
    pub aspect_ratios: Vec<String>,
    pub default_resolution: String,
    pub default_aspect_ratio: String,
    pub max_images: Option<usize>,
    pub max_audios: Option<usize>,
    pub max_videos: Option<usize>,
}

impl Default for VideoWorkflowConfig {
    fn default() -> Self {
        let s = |v: &str| v.to_string();
        Self {
            active: false,
            i2v_node: s("generator_i2v"),
            r2v_node: s("generator_r2v"),
            generator_node: s("generator"),
            prompt_field: s("prompt"),
            width_field: s("width"),
            height_field: s("height"),
            length_field: s("length"),
            first_frame_field: s("first_frame"),
            last_frame_field: s("last_frame"),
            image_prefix: s("ref_images."),
            audio_prefix: s("ref_audios."),
            video_prefix: s("ref_videos."),
            seed_node: s("rnd_seed"),
            seed_field: s("noise_seed"),
            denoise_node: s("scheduler"),
            output_node: s("video_output"),
            min_duration: 5,
            max_duration: 15,
            default_duration: 5,
            fps: 24,
            frame_step: 17,
            frame_offset: 5,
            size_multiple: 64,
            resolutions: ["360p", "480p", "720p", "768p", "1080p"].map(s).to_vec(),
            aspect_ratios: ["16:9", "9:16", "1:1", "4:3", "3:4", "3:2", "2:3", "21:9", "9:21"].map(s).to_vec(),
            default_resolution: s("480p"),
            default_aspect_ratio: s("16:9"),
            max_images: None,
            max_audios: None,
            max_videos: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub comfyui_url: String,
    pub llm: Option<LlmConfig>,
    pub workflows: HashMap<String, WorkflowConfig>,
    #[serde(default)]
    pub video_workflows: HashMap<String, VideoWorkflowConfig>,
    #[serde(default)]
    pub enable_openai_compat: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            comfyui_url: "http://127.0.0.1:8188".to_string(),
            llm: None,
            workflows: HashMap::new(),
            video_workflows: HashMap::new(),
            enable_openai_compat: false,
        }
    }
}

pub fn load_config<P: AsRef<Path>>(path: P) -> Config {
    if let Ok(content) = fs::read_to_string(path.as_ref()) {
        toml::from_str(&content).unwrap_or_default()
    } else {
        Config::default()
    }
}

pub fn save_config<P: AsRef<Path>>(config: &Config, path: P) -> Result<(), std::io::Error> {
    let toml_str = toml::to_string(config).unwrap();
    fs::write(path, toml_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_field_input_target_serialization() {
        let field_map = WorkflowFieldMap {
            original_node_id: "1".to_string(),
            original_field_name: "image".to_string(),
            exposed_as: "input_image".to_string(),
            required: true,
            input_target: FieldInputTarget::ImageBase64,
            randomize: false,
            is_value_map: false,
            map_keys: "".to_string(),
            map_values: "".to_string(),
        };

        let serialized = toml::to_string(&field_map).unwrap();
        assert!(serialized.contains("input_target = \"image_base64\""));

        let deserialized: WorkflowFieldMap = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.input_target, FieldInputTarget::ImageBase64);
    }

    #[test]
    fn test_field_input_target_default_deserialization() {
        // Test deserializing from old config format that lacks input_target
        let old_toml = r#"
            original_node_id = "1"
            original_field_name = "text"
            exposed_as = "prompt"
        "#;
        let deserialized: WorkflowFieldMap = toml::from_str(old_toml).unwrap();
        assert_eq!(deserialized.input_target, FieldInputTarget::Text);
    }

    #[test]
    fn test_image_inputs_optional() {
        let old: WorkflowConfig = toml::from_str("active = true\nfile_name = \"a\"\nexposed_fields = []").unwrap();
        assert!(old.image_inputs.is_none());

        let new: WorkflowConfig = toml::from_str(
            "active = true\nfile_name = \"a\"\nexposed_fields = []\n[image_inputs]\nnode_id = \"prompt\"\nfield_prefix = \"images.image_\"\nmax_images = 3",
        ).unwrap();
        let cfg = new.image_inputs.unwrap();
        assert_eq!(cfg.node_id, "prompt");
        assert_eq!(cfg.max_images, Some(3));
    }

    #[test]
    fn test_video_workflows() {
        let old: Config = toml::from_str("comfyui_url = \"x\"\n[workflows]").unwrap();
        assert!(old.video_workflows.is_empty());

        let cfg: Config = toml::from_str(
            "comfyui_url = \"x\"\n[workflows]\n[video_workflows.minimax_h3]\nactive = true\n[video_workflows.other]\nactive = true\nmax_duration = 10\nresolutions = [\"720p\"]",
        ).unwrap();
        let minimax = &cfg.video_workflows["minimax_h3"];
        assert_eq!(minimax, &VideoWorkflowConfig { active: true, ..Default::default() });
        let other = &cfg.video_workflows["other"];
        assert_eq!(other.max_duration, 10);
        assert_eq!(other.resolutions, vec!["720p"]);
        assert_eq!(other.i2v_node, "generator_i2v");

        // Survives a save/load round trip (the dashboard posts the whole config back)
        let again: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(&again.video_workflows["other"], other);
    }
}
