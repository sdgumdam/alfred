//! 行为契约与沙箱档案（施工清单 §2.1/§2.3/§2.5，限界上下文 §6.3/§6.3.1）。

use serde::{Deserialize, Serialize};

/// 行为契约：给执行者的明确工作要求（限界上下文 §6.3）。
///
/// - `prompt`：给执行者（pi）的任务描述。
/// - `acceptance_criteria`：给审查者的验收标准（执行审查的判分依据）。
/// - `reviewer_models`：异构审查模型列表。**系统注入**（实施计划 E5：
///   规划器不感知审查者，add_node 后由系统从 config roles.reviewer 注入）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub prompt: String,
    pub acceptance_criteria: String,
    #[serde(default)]
    pub reviewer_models: Vec<String>,
}

/// 沙箱档案：agent 节点的执行环境声明（限界上下文 §6.3.1）。
///
/// 契约管"做什么"，档案管"在什么约束下做"。planner 声明、计划审查按
/// 最小权限审、编排器起容器时照档案执行。**工作区本身由执行驱动固定
/// 挂载，不在此列**（施工清单 §2.5）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxProfile {
    /// 工作区之外的挂载卷（一律只读，用于参考材料；不 cp 进工作区）。
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
    /// 语言运行时（如 "rust" / "python"）。缺省 = 容器默认镜像。
    #[serde(default)]
    pub runtime: Option<String>,
    /// 需要安装的依赖包。
    #[serde(default)]
    pub packages: Vec<String>,
    /// 是否允许联网。**默认拒绝**（施工清单 §2.5 / P2）。
    #[serde(default)]
    pub network: bool,
    /// 契约声明的工作区子目录（相对持久 ws 的**相对路径**；R6a/M5 显式声明制）。
    /// executor 只挂这些子目录（空 = 不挂 ws），且必须相对、不含 `..`——
    /// 绝对/越界路径在 compose 生成时被拒绝（防静默换基 rw 挂载）。
    #[serde(default)]
    pub workspace_subdirs: Vec<String>,
}

impl Default for SandboxProfile {
    fn default() -> Self {
        Self {
            volumes: Vec::new(),
            runtime: None,
            packages: Vec::new(),
            network: false,
            workspace_subdirs: Vec::new(),
        }
    }
}

/// 挂载卷（限界上下文 §6.3.1）。
///
/// **反序列化容错（原始用例修复）**：pi 建图偶尔按教学输出的字段名写
/// `readonly` 别名（实测 `readonly: true`）——规范 schema `deny_unknown_fields`
/// 直接拒绝会把真需求打成 planning_error_escalated。这里接受常见别名并归一
/// 成 `mode`；规范 `mode` 字段在场即权威；未知字段仍拒（防真错）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VolumeMount {
    /// 宿主机路径（planner 申请的范围，计划审查按最小权限审）。
    pub host_path: String,
    /// 容器内挂载路径（建议独立路径如 /references，不与工作区混）。
    pub container_path: String,
    /// 挂载模式：只许 `"ro"`（缺省 ro——参考卷一律只读，契约 §2.5：不 cp 进
    /// 工作区）。非 ro 值在执行侧 `validate_ref_volume` 显式拒绝。
    pub mode: String,
}

/// `VolumeMount.mode` 缺省值（"ro"——无任何 mode/别名字段时的缺省；参考卷
/// 一律只读）。
fn default_volume_mode() -> String {
    "ro".to_string()
}

/// `readonly` / `read_only` 别名的取值形态（bool 或字符串，untagged）。
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum VolumeModeAlias {
    Bool(bool),
    Str(String),
}

impl VolumeModeAlias {
    /// 归一成 `mode`：`true`/"ro"/"readonly"/"read-only"/"read_only" → "ro"；
    /// `false`/"rw"/"readwrite"/"read-write"/"read_write" → "rw"。`false` 照实
    /// 落 "rw"（语义不静默改写）——参考卷只读闸门由执行侧
    /// `validate_ref_volume` 对非 ro 显式拒绝，解析层绝不悄悄升 ro。
    fn into_mode(self) -> Result<String, String> {
        match self {
            VolumeModeAlias::Bool(true) => Ok("ro".to_string()),
            VolumeModeAlias::Bool(false) => Ok("rw".to_string()),
            VolumeModeAlias::Str(s) => match s.trim().to_lowercase().as_str() {
                "ro" | "readonly" | "read-only" | "read_only" => Ok("ro".to_string()),
                "rw" | "readwrite" | "read-write" | "read_write" => Ok("rw".to_string()),
                other => Err(format!(
                    "unknown volume mode value '{other}' (expected ro/rw, or readonly: true/false)"
                )),
            },
        }
    }
}

/// 反序列化中间形态：`deny_unknown_fields` 保留在中间层——规范字段之外只多认
/// `readonly` / `read_only` 两个别名，其余未知字段仍报错（字段漂移防线不变）。
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VolumeMountInput {
    host_path: String,
    container_path: String,
    /// 规范字段：在场即权威（原样透传，即便与别名并存——矛盾交给执行侧显式
    /// 拒绝，解析层不改写 canonical 值）；缺省 ro。
    #[serde(default)]
    mode: Option<String>,
    /// 教学输出常见别名（bool：true→ro / false→rw）。
    #[serde(default)]
    readonly: Option<VolumeModeAlias>,
    /// 同上（蛇形拼写）。
    #[serde(default)]
    read_only: Option<VolumeModeAlias>,
}

impl VolumeMountInput {
    fn into_volume_mount(self) -> Result<VolumeMount, String> {
        let mode = if let Some(mode) = self.mode {
            mode
        } else if let Some(alias) = self.readonly {
            alias.into_mode()?
        } else if let Some(alias) = self.read_only {
            alias.into_mode()?
        } else {
            default_volume_mode()
        };
        Ok(VolumeMount {
            host_path: self.host_path,
            container_path: self.container_path,
            mode,
        })
    }
}

impl<'de> serde::Deserialize<'de> for VolumeMount {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        VolumeMountInput::deserialize(deserializer)?
            .into_volume_mount()
            .map_err(serde::de::Error::custom)
    }
}
