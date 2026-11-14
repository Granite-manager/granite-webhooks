//! Raptor (LPI Cloud, templator.com) has no public API. This is the JSON API
//! its own website calls (`api.templator.com/api/web`, Bearer token), as the
//! site sent it on 2026-11-13:
//!
//! - `POST /auth/login` → token; `GET /user/bootstrap` → the Raptor company;
//!   `GET /entity/{company}/bootstrap` → its "Work" folder (type 1000), where
//!   jobs live; `GET /company/{company}/settings` → default edge style set
//!   and pickup location
//! - `GET /company/{company}/jobs` → the job list (deleted jobs included)
//! - `POST /job/{work}/new` → a new job; `GET /job/{id}` → its entities, of
//!   which type 404 holds customer, address, materials and cutouts and is
//!   saved whole with `PUT /entity/{id}`
//! - `POST /job/{id}/note` → a note; `GET /job/{id}/note` → its notes
//! - `PUT /entity/{id}` also saves a note (type 410) or renames the job
//!   itself (type 400), as tried on a TEST job on 2026-11-13

use super::plan::{Job, Plan};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

/// Raptor's API; `RAPTOR_API_URL` may point elsewhere.
pub const API: &str = "https://api.templator.com/api/web";
/// Raptor's "Pending Measure", the status a new job gets in its Add Job form.
const PENDING_MEASURE: i64 = 101;

pub type Result<T> = std::result::Result<T, String>;

/// Which parts of Job Info to write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Parts {
    pub customer: bool,
    pub address: bool,
    pub materials: bool,
    pub cutouts: bool,
}

impl Parts {
    pub const ALL: Self = Self {
        customer: true,
        address: true,
        materials: true,
        cutouts: true,
    };

    pub const fn any(self) -> bool {
        self.customer || self.address || self.materials || self.cutouts
    }
}

/// A job in Raptor's list.
#[derive(Debug, Clone, Deserialize)]
pub struct ListJob {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub reference: Option<String>,
    /// "Active" or "Deleted".
    #[serde(default)]
    pub status: Option<String>,
}

impl ListJob {
    pub fn active(&self) -> bool {
        self.status.as_deref() != Some("Deleted")
    }
}

/// A signed-in Raptor company.
pub struct Raptor {
    http: reqwest::Client,
    base: String,
    token: String,
    user_id: String,
    company: String,
    work: String,
    edge_set: Value,
    staging: Value,
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(v, |v, key| v.get(key))?.as_str()
}

impl Raptor {
    /// Signs in at `base` and finds the company, its jobs folder and its defaults.
    pub async fn login(
        http: reqwest::Client,
        base: &str,
        email: &str,
        password: &str,
    ) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        let response = http
            .post(format!("{base}/auth/login"))
            .json(&json!({ "email": email, "password": password }))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| format!("Raptor login: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Raptor did not accept the login ({})",
                response.status().as_u16()
            ));
        }
        let login: Value = response
            .json()
            .await
            .map_err(|e| format!("Raptor login: {e}"))?;
        let token = str_at(&login, &["token"])
            .ok_or("Raptor login gave no token")?
            .to_string();
        let user_id = str_at(&login, &["id"]).unwrap_or_default().to_string();
        let mut raptor = Self {
            http,
            base,
            token,
            user_id,
            company: String::new(),
            work: String::new(),
            edge_set: Value::Null,
            staging: Value::Null,
        };
        let user = raptor.get("/user/bootstrap").await?;
        raptor.company = user["companies"][0]["id"]
            .as_str()
            .ok_or("Raptor login has no company")?
            .to_string();
        let boot = raptor
            .get(&format!("/entity/{}/bootstrap", raptor.company))
            .await?;
        raptor.work = boot["rootContainers"]
            .as_array()
            .and_then(|list| list.iter().find(|c| c["entityTypeId"] == 1000))
            .and_then(|c| c["id"].as_str())
            .ok_or("Raptor company has no Work folder")?
            .to_string();
        let settings = raptor
            .get(&format!("/company/{}/settings", raptor.company))
            .await?;
        let attributes = &settings["attributes"];
        raptor.edge_set = attributes["designer"]["edgeStyle"]["defaultEdgeStyleSet"].clone();
        raptor.staging = attributes["job"]["defaultStagingLocationId"].clone();
        Ok(raptor)
    }

    async fn send(&self, request: reqwest::RequestBuilder, what: &str) -> Result<Value> {
        let response = request
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("Raptor {what}: {e}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail: String = text.chars().take(200).collect();
            return Err(format!("Raptor {what}: {} {detail}", status.as_u16()));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.send(self.http.get(format!("{}{path}", self.base)), path)
            .await
    }

    /// Every job, deleted ones included.
    pub async fn jobs(&self) -> Result<Vec<ListJob>> {
        let mut all = Vec::new();
        for offset in (0..).step_by(200) {
            let page = self
                .get(&format!(
                    "/company/{}/jobs?status=All&sortBy=9&sortDirection=1&offset={offset}&limit=200",
                    self.company
                ))
                .await?;
            let jobs: Vec<ListJob> = serde_json::from_value(page["jobs"].clone())
                .map_err(|e| format!("Raptor job list: {e}"))?;
            let done = jobs.len() < 200;
            all.extend(jobs);
            if done || offset > 20_000 {
                break;
            }
        }
        Ok(all)
    }

    /// Adds a job as the Add Job form does; its id.
    pub async fn create_job(&self, reference: &str, name: &str) -> Result<String> {
        let body = json!({
            "name": name,
            "reference": reference,
            "description": "",
            "jobStatusId": PENDING_MEASURE,
            "edgeStyleSetId": self.edge_set,
            "stagingAddressId": self.staging,
            "allowedUsers": [],
            "pageCollection": {
                "pages": [{
                    "projector": {
                        "name": "3-point-least-squares-plane.v1",
                        "transformations": [], "points": [], "pitch": 0.0, "roll": 0.0,
                    },
                    "name": "",
                }],
                "capturePageCount": 1, "drawingPageCount": 0, "totalPageCount": 1,
            },
        });
        let created = self
            .send(
                self.http
                    .post(format!("{}/job/{}/new", self.base, self.work))
                    .json(&body),
                "add job",
            )
            .await?;
        created["uuid"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "Raptor add job gave no id".to_string())
    }

    /// Job Info, Material and Cutout, saved like the job's Details page.
    pub async fn fill(&self, job_id: &str, plan: &Plan, job: &Job) -> Result<()> {
        self.update_job(job_id, None, plan, job, Parts::ALL).await
    }

    /// Renames the job (`name`) and writes `parts` of its Job Info; the rest
    /// stays as the office left it.
    pub async fn update_job(
        &self,
        job_id: &str,
        name: Option<&str>,
        plan: &Plan,
        job: &Job,
        parts: Parts,
    ) -> Result<()> {
        let entities = self.get(&format!("/job/{job_id}?activeOnly=True")).await?;
        let find = |kind: i64| {
            entities
                .as_array()
                .and_then(|list| list.iter().find(|e| e["entityTypeId"] == kind))
                .cloned()
        };
        if let Some(name) = name {
            let mut entity = find(400).ok_or("Raptor job is gone")?;
            entity["name"] = json!(name);
            self.put_entity(&entity, "rename job").await?;
        }
        if parts.any() {
            let mut info = find(404).ok_or("Raptor job has no Job Info")?;
            info_attributes(&mut info["attributes"], plan, job, &self.edge_set, parts);
            self.put_entity(&info, "save job info").await?;
        }
        Ok(())
    }

    /// Deletes a job as the job list's Delete does (Raptor keeps it as
    /// "Deleted").
    pub async fn delete_job(&self, job_id: &str) -> Result<()> {
        self.send(
            self.http
                .put(format!("{}/entity/{job_id}/status", self.base))
                .json(&json!({ "status": 10, "setChildren": false })),
            "delete job",
        )
        .await?;
        Ok(())
    }

    async fn put_entity(&self, entity: &Value, what: &str) -> Result<Value> {
        let id = entity["id"].as_str().unwrap_or_default();
        self.send(
            self.http
                .put(format!("{}/entity/{id}", self.base))
                .json(entity),
            what,
        )
        .await
    }

    /// Puts the lines that changed from `old` to `new` into the job's "Notes"
    /// note (`note_id`, else the first one titled "Notes"); the office's own
    /// lines stay. Nothing when the office deleted the note.
    pub async fn update_note(
        &self,
        job_id: &str,
        note_id: Option<&str>,
        old: &str,
        new: &str,
    ) -> Result<()> {
        let notes = self.get(&format!("/job/{job_id}/note")).await?;
        let list = notes.as_array().cloned().unwrap_or_default();
        let found = note_id
            .and_then(|id| list.iter().find(|n| n["id"] == id))
            .or_else(|| list.iter().find(|n| n["attributes"]["title"] == "Notes"));
        let Some(mut note) = found.cloned() else {
            return Ok(());
        };
        let body = note["attributes"]["body"].as_str().unwrap_or_default();
        let merged = merge_note(body, old, new);
        if merged == body {
            return Ok(());
        }
        note["attributes"]["body"] = json!(merged);
        self.put_entity(&note, "save note").await?;
        Ok(())
    }

    /// The "Notes" note, at Pending Measure like the office's; its id.
    pub async fn add_note(&self, job_id: &str, body: &str) -> Result<String> {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let zero = "00000000-0000-0000-0000-000000000000";
        let id = uuid::Uuid::new_v4().to_string();
        let note = json!({
            "id": id,
            "entityTypeId": 410,
            "companyId": null,
            "parentId": zero,
            "name": null, "description": null, "tags": null,
            "attributes": {
                "title": "Notes",
                "body": body,
                "workflowStageId": PENDING_MEASURE,
                "pageId": null, "photoIds": [], "templateId": null, "migrated": false,
            },
            "attachments": null, "version": null, "revision": zero, "status": 0,
            "createdAt": now, "createdBy": self.user_id,
            "updatedAt": now, "updatedBy": zero,
            "isDeleted": false, "children": null, "counts": null,
        });
        self.send(
            self.http
                .post(format!("{}/job/{job_id}/note", self.base))
                .json(&note),
            "add note",
        )
        .await?;
        Ok(id)
    }
}

/// A note line's label, "10-year sealer" and "10 year sealer" alike.
fn label(line: &str) -> Option<String> {
    let (label, _) = line.split_once(':')?;
    let label = label.trim().to_lowercase().replace("10-year", "10 year");
    (!label.is_empty()).then_some(label)
}

fn value(line: &str) -> &str {
    line.split_once(':').map_or("", |(_, v)| v.trim())
}

/// `current` with each line whose value went from `old` to `new` replaced
/// (or added after the last line); every other line as it is.
pub fn merge_note(current: &str, old: &str, new: &str) -> String {
    let before: HashMap<String, &str> = old
        .lines()
        .filter_map(|l| Some((label(l)?, value(l))))
        .collect();
    let mut lines: Vec<String> = current
        .replace("\r\n", "\n")
        .split('\n')
        .map(String::from)
        .collect();
    for line in new.lines() {
        let Some(key) = label(line) else {
            continue;
        };
        if before.get(&key).copied().unwrap_or_default() == value(line) {
            continue;
        }
        if let Some(at) = lines
            .iter()
            .position(|l| label(l).as_deref() == Some(key.as_str()))
        {
            lines[at] = line.to_string();
        } else {
            let end = lines
                .iter()
                .rposition(|l| !l.trim().is_empty())
                .map_or(0, |i| i + 1);
            lines.insert(end, line.to_string());
        }
    }
    lines.join("\n")
}

/// The next "#N" after every numbered job, deleted ones too.
pub fn next_number(jobs: &[ListJob]) -> u32 {
    jobs.iter()
        .filter_map(|j| {
            j.reference
                .as_deref()?
                .trim()
                .strip_prefix('#')?
                .parse::<u32>()
                .ok()
        })
        .max()
        .unwrap_or(0)
        + 1
}

fn typed(kind: &str, value: Value) -> Value {
    let mut typed = serde_json::Map::new();
    typed.insert("type".into(), Value::String(kind.into()));
    typed.insert("value".into(), value);
    Value::Object(typed)
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Writes the plan's `parts` into a job's Job Info attributes: customer,
/// address, materials and cutouts. Anything else Raptor keeps there stays.
pub fn info_attributes(
    attributes: &mut Value,
    plan: &Plan,
    job: &Job,
    edge_set: &Value,
    parts: Parts,
) {
    if !attributes.is_object() {
        *attributes = json!({});
    }
    if parts.customer {
        customer_attributes(attributes, plan);
    }
    if parts.address {
        address_attributes(attributes, plan);
    }
    if parts.materials || parts.cutouts {
        category_attributes(attributes, job, edge_set, parts);
    }
}

fn customer_attributes(attributes: &mut Value, plan: &Plan) {
    let customer = &mut attributes["customer"];
    if !customer.is_object() {
        *customer = json!({ "name": "Customer", "id": new_id(), "properties": {} });
    }
    customer["properties"]["name"] = typed("Text", json!(plan.customer_name));
    customer["properties"]["phone"] = typed("PhoneUS", json!(plan.phone));
    if !customer["properties"]["email"].is_object() {
        customer["properties"]["email"] = typed("Email", json!(""));
    }
}

fn address_attributes(attributes: &mut Value, plan: &Plan) {
    let job_info = &mut attributes["jobInfo"];
    if !job_info.is_object() {
        *job_info = json!({ "name": "JobInfo", "id": new_id(), "properties": {} });
    }
    let a = &plan.address;
    job_info["properties"]["address"] = typed(
        "Address",
        json!({
            "addressLine1": a.street, "addressLine2": "", "locality": a.city,
            "region": a.state, "postalCode": a.zip, "country": "USA", "isValidAddress": true,
        }),
    );
}

#[allow(clippy::too_many_lines)]
fn category_attributes(attributes: &mut Value, job: &Job, edge_set: &Value, parts: Parts) {
    let materials: Vec<Value> = job
        .materials
        .iter()
        .map(|m| {
            json!({
                "name": "Material",
                "id": new_id(),
                "properties": {
                    "thickness": typed("Uom", json!({ "value": 3.0, "uom": 1 })),
                    "brand": typed("Text", json!(m.brand)),
                    "color": typed("Text", json!(m.color)),
                    "slabSize": typed("Size", Value::Null),
                    "drawings": typed("DrawingIds", json!([])),
                    "edgeStyleSelection": typed("EdgeStyles", json!({
                        "edgeStyleSet": edge_set,
                        "edgeStyles": [],
                        "primaryEdgeName": typed("Text", json!(m.edge)),
                    })),
                    "supports": typed("Properties", json!({ "isOn": typed("Bool", json!(false)) })),
                },
            })
        })
        .collect();
    let cutouts: Vec<Value> = job
        .cutouts
        .iter()
        .map(|c| {
            json!({
                "name": "Cutout",
                "id": new_id(),
                "properties": { "cutoutType": typed("Properties", json!({
                    "customerSupplied": typed("Bool", json!(false)),
                    "sinkOnsite": typed("Bool", json!(false)),
                    "location": typed("DrawingIds", json!([])),
                    "brand": typed("Text", json!("")),
                    "model": typed("Text", json!(c.model)),
                    "farmSink": typed("Properties", json!({ "isOn": typed("Bool", json!(false)) })),
                    "setback": typed("Uom", Value::Null),
                    "quantity": typed("Integer", json!(c.quantity)),
                    "sinkMountType": typed("Properties", json!({
                        "reveal": typed("Uom", Value::Null),
                        "radioOption": typed("Text", json!("sinkUndermount")),
                    })),
                    "size": typed("Size", Value::Null),
                    "tapHoles": typed("Properties", json!({
                        "diameter": typed("Uom", Value::Null),
                        "quantity": typed("Integer", json!(0)),
                        "spread": typed("Uom", Value::Null),
                        "drilledOnsite": typed("Bool", json!(false)),
                    })),
                    "notes": typed("Text", json!("")),
                    "radioOption": typed("Text", json!("cutoutSink")),
                })) },
            })
        })
        .collect();
    if !attributes["categories"].is_object() {
        attributes["categories"] = json!({});
    }
    let categories = &mut attributes["categories"];
    // An empty list only replaces a list that is there.
    if parts.materials && (!materials.is_empty() || categories.get("material").is_some()) {
        categories["material"] = Value::Array(materials);
    }
    if parts.cutouts && (!cutouts.is_empty() || categories.get("cutout").is_some()) {
        categories["cutout"] = Value::Array(cutouts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raptor::plan::{Address, Cutout, Material};

    fn list(refs: &[(&str, &str)]) -> Vec<ListJob> {
        refs.iter()
            .map(|(r, s)| ListJob {
                id: String::new(),
                name: String::new(),
                reference: Some((*r).to_string()),
                status: Some((*s).to_string()),
            })
            .collect()
    }

    #[test]
    fn next_number_follows_every_numbered_job() {
        assert_eq!(
            next_number(&list(&[
                ("#343", "Active"),
                ("TEST", "Active"),
                ("#344 ", "Deleted"),
                ("Kitchen", "Active")
            ])),
            345
        );
        assert_eq!(next_number(&[]), 1);
    }

    #[test]
    fn job_info_keeps_what_raptor_had() {
        let plan = Plan {
            customer_name: "Mike Pendley".into(),
            phone: "317-414-6755".into(),
            address: Address {
                street: "8435 Smithfield Lane".into(),
                city: "Indianapolis".into(),
                state: "IN".into(),
                zip: "46237".into(),
            },
            jobs: vec![],
        };
        let job = Job {
            name: "Pendley Mike Kitchen".into(),
            sq_ft: 44.4,
            materials: vec![Material {
                brand: "Dolomite".into(),
                color: "Fantasy Brown 3".into(),
                edge: "Flat".into(),
            }],
            cutouts: vec![Cutout {
                model: "30\" Farmer Sink Cutout".into(),
                quantity: 1,
            }],
            notes: String::new(),
        };
        let mut attributes = json!({
            "account": {"name": "Account", "id": "a", "properties": {}},
            "customer": {"name": "Customer", "id": "c", "properties": {"email": {"type": "Email", "value": "x@y.z"}}},
            "jobInfo": {"name": "JobInfo", "id": "j", "properties": {"jobByDate": {"type": "Properties", "value": {}}}},
            "categories": {},
        });
        info_attributes(&mut attributes, &plan, &job, &json!("set-1"), Parts::ALL);
        assert_eq!(attributes["account"]["id"], "a");
        assert_eq!(attributes["customer"]["id"], "c");
        assert_eq!(
            attributes["customer"]["properties"]["name"]["value"],
            "Mike Pendley"
        );
        assert_eq!(
            attributes["customer"]["properties"]["email"]["value"],
            "x@y.z"
        );
        assert!(attributes["jobInfo"]["properties"]["jobByDate"].is_object());
        let address = &attributes["jobInfo"]["properties"]["address"]["value"];
        assert_eq!(address["addressLine1"], "8435 Smithfield Lane");
        assert_eq!(address["region"], "IN");
        let material = &attributes["categories"]["material"][0]["properties"];
        assert_eq!(material["color"]["value"], "Fantasy Brown 3");
        assert_eq!(material["thickness"]["value"]["value"], 3.0);
        assert_eq!(
            material["edgeStyleSelection"]["value"]["edgeStyleSet"],
            "set-1"
        );
        let cutout = &attributes["categories"]["cutout"][0]["properties"]["cutoutType"]["value"];
        assert_eq!(cutout["model"]["value"], "30\" Farmer Sink Cutout");
        assert_eq!(cutout["quantity"]["value"], 1);
        // Only the parts asked for.
        let mut attributes = json!({"categories": {"material": [{"id": "m"}]}});
        let parts = Parts {
            cutouts: true,
            ..Parts::default()
        };
        info_attributes(&mut attributes, &plan, &job, &json!("set-1"), parts);
        assert_eq!(attributes["categories"]["material"][0]["id"], "m");
        assert!(attributes.get("customer").is_none());
        assert_eq!(
            attributes["categories"]["cutout"].as_array().map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn a_note_keeps_the_office_lines() {
        let old =
            "Floor:\nBacksplash:4\"\nSeam:STD\nAdditional notes:\nSalesperson:Liza\nLayout:\n";
        let new =
            "Floor:\nBacksplash:FH\nSeam:STD\nAdditional notes:\nSalesperson:Liza\nLayout:yes\n";
        let current = "Floor:2nd\nBacksplash:4\"\nSeam:Phantom\nAdditional notes:\ncall first\nSalesperson:Liza\n\n";
        assert_eq!(
            merge_note(current, old, new),
            "Floor:2nd\nBacksplash:FH\nSeam:Phantom\nAdditional notes:\ncall first\nSalesperson:Liza\nLayout:yes\n\n"
        );
        assert_eq!(merge_note(current, old, old), current);
        // The short pick-up note spells it "10-year sealer".
        assert_eq!(
            merge_note(
                "10-year sealer:Yes\n",
                "10 year sealer:Yes\n",
                "10-year sealer:Yes\n"
            ),
            "10-year sealer:Yes\n"
        );
    }
}
