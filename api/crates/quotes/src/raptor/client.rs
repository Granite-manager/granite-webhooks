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
//! - `POST /job/{id}/note` → a note

use super::plan::{Job, Plan};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

/// Raptor's API; `RAPTOR_API_URL` may point elsewhere.
pub const API: &str = "https://api.templator.com/api/web";
/// Raptor's "Pending Measure", the status a new job gets in its Add Job form.
const PENDING_MEASURE: i64 = 101;

pub type Result<T> = std::result::Result<T, String>;

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
        let entities = self.get(&format!("/job/{job_id}?activeOnly=True")).await?;
        let mut info = entities
            .as_array()
            .and_then(|list| list.iter().find(|e| e["entityTypeId"] == 404))
            .cloned()
            .ok_or("Raptor job has no Job Info")?;
        let id = info["id"].as_str().unwrap_or_default().to_string();
        info_attributes(&mut info["attributes"], plan, job, &self.edge_set);
        self.send(
            self.http
                .put(format!("{}/entity/{id}", self.base))
                .json(&info),
            "save job info",
        )
        .await?;
        Ok(())
    }

    /// The "Notes" note, at Pending Measure like the office's.
    pub async fn add_note(&self, job_id: &str, body: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let zero = "00000000-0000-0000-0000-000000000000";
        let note = json!({
            "id": uuid::Uuid::new_v4().to_string(),
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
        Ok(())
    }
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

/// Writes the plan into a job's Job Info attributes: customer, address,
/// materials and cutouts. Anything else Raptor keeps there stays.
pub fn info_attributes(attributes: &mut Value, plan: &Plan, job: &Job, edge_set: &Value) {
    if !attributes.is_object() {
        *attributes = json!({});
    }
    let customer = &mut attributes["customer"];
    if !customer.is_object() {
        *customer = json!({ "name": "Customer", "id": new_id(), "properties": {} });
    }
    customer["properties"]["name"] = typed("Text", json!(plan.customer_name));
    customer["properties"]["phone"] = typed("PhoneUS", json!(plan.phone));
    if !customer["properties"]["email"].is_object() {
        customer["properties"]["email"] = typed("Email", json!(""));
    }
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
    if !materials.is_empty() {
        categories["material"] = Value::Array(materials);
    }
    if !cutouts.is_empty() {
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
        info_attributes(&mut attributes, &plan, &job, &json!("set-1"));
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
    }
}
