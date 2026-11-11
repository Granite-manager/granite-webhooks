use super::access::{Permissions, Stage};
use crate::openapi::{IntoParams, ToSchema};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// A project, with its transaction's milestones. Dates are UTC ISO strings.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Project {
    pub id: i32,
    /// The won deal it came from.
    pub source_deal_id: i64,
    pub customer_id: i32,
    /// Its transaction, once linked.
    pub sale_id: Option<i32>,
    pub sales_rep_id: Option<i32>,
    pub owner_id: Option<i32>,
    pub title: String,
    pub quoted_amount: Option<f64>,
    pub project_address: Option<String>,
    /// The stage, Installed and Paid & Completed read from the transaction.
    #[sqlx(try_from = "String")]
    pub stage_code: Stage,
    pub position: i32,
    pub is_on_hold: bool,
    pub cancelled_at: Option<String>,
    pub templated_at: Option<String>,
    /// The shop marked it fabricated.
    pub fabricated_at: Option<String>,
    /// Its template is on the calendar for this time (UTC ISO), not done yet.
    pub template_date: Option<String>,
    /// Its installation is on the calendar for this time (UTC ISO), not done yet.
    pub install_date: Option<String>,
    pub archived_at: Option<String>,
    /// Bumped on every change; actions send it back to catch a stale page.
    pub version: i32,
    pub created_at: String,
    pub updated_at: String,
    pub installed_at: Option<String>,
    pub paid_at: Option<String>,
    /// The transaction was cancelled (the project counts as cancelled).
    pub sale_cancelled_at: Option<String>,
    pub transaction_price: Option<f64>,
    pub customer_name: String,
    pub owner_name: Option<String>,
    pub sales_rep_name: Option<String>,
    /// The deal's quote (its sold one, else its newest): the card's name.
    pub quote_id: Option<i32>,
    pub quote_name: Option<String>,
    pub customer_phone: Option<String>,
    pub customer_email: Option<String>,
    pub customer_company: Option<String>,
    /// Every active slab of the transaction is cut.
    pub ready_to_install: bool,
}

/// Someone in the company.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Person {
    /// Its id.
    pub id: i32,
    /// Its name.
    pub name: String,
}

/// A task on a project, for anyone in the company.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ProjectTask {
    /// Its id.
    pub id: i32,
    /// The project it belongs to.
    pub project_id: i32,
    /// Its name.
    pub name: String,
    /// Who does it; null for nobody.
    pub assigned_user_id: Option<i32>,
    /// That person's name.
    pub assigned_user_name: Option<String>,
    /// Store wall time `YYYY-MM-DDTHH:MM:SS`; `T00:00:00` is a date with no time.
    pub deadline: Option<String>,
    /// `low`, `medium` or `high` (no longer shown; new tasks are `medium`).
    pub priority: String,
    /// When it was done, UTC ISO; null while open.
    pub completed_at: Option<String>,
    /// Who made it.
    pub created_by: i32,
    /// Its maker's name.
    pub created_by_name: Option<String>,
    /// When it was made, UTC ISO.
    pub created_at: String,
}

/// The next template or installation appointment.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ProjectAppointment {
    /// Its id.
    pub id: i32,
    /// The project it belongs to.
    pub project_id: i32,
    /// `templates` or `installations` (the calendar).
    pub kind: String,
    /// The event title.
    pub title: String,
    /// When it starts, UTC ISO.
    pub start_date: String,
    /// Waiting for a date on the schedule (`start_date` means nothing yet).
    pub unscheduled: bool,
}

/// Query of `GET /v1/projects`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BoardQuery {
    /// `active` (default), `completed` or `cancelled`.
    pub view: Option<String>,
    /// An owner's user id, or `unassigned`.
    pub owner: Option<String>,
    /// Title, customer or address contains this.
    pub q: Option<String>,
    /// Only projects on hold.
    #[serde(default)]
    pub hold: bool,
    /// Include archived projects.
    #[serde(default)]
    pub archived: bool,
    /// Asked from the employee side, where being an admin counts for nothing.
    #[serde(default)]
    pub employee: bool,
    /// 1-based page of 100.
    pub page: Option<u32>,
}

/// `GET /v1/projects`: the caller's board.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectsBoard {
    /// The projects, in board order.
    pub projects: Vec<Project>,
    /// The columns the caller has, in order.
    pub stages: Vec<Stage>,
    /// People who may own a project (the owner filter).
    pub owners: Vec<Person>,
    /// The next appointments of these projects.
    pub appointments: Vec<ProjectAppointment>,
    /// Their open tasks, the most urgent first.
    pub tasks: Vec<ProjectTask>,
    /// The company's own steps in the process.
    pub process_steps: Vec<ProcessStep>,
    /// Which of those steps these projects have done.
    pub done_steps: Vec<DoneStep>,
    /// How many issues the crew reported on these projects' jobs (only those with any).
    pub issue_counts: Vec<ProjectIssueCount>,
    /// What the caller may do.
    pub permissions: Permissions,
    /// This page, from 1.
    pub page: u32,
    /// Another page follows.
    pub has_next: bool,
}

/// A step a company added to a stage of its process.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ProcessStep {
    /// Its id.
    pub id: i32,
    /// The stage it belongs to.
    #[sqlx(try_from = "String")]
    pub stage_code: Stage,
    /// What to do, e.g. "Confirm a file".
    pub name: String,
    /// Its place in the stage.
    pub position: i32,
    /// Who is told when a project checks it off (user ids).
    #[sqlx(skip)]
    pub notify: Vec<i32>,
    /// Also tells the sales rep assigned to the project.
    pub notify_sales_rep: bool,
    /// What checks it off by itself, if anything.
    #[sqlx(skip)]
    pub auto: Option<StepAuto>,
}

/// What checks a process step off by itself.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StepAuto {
    /// What happens: `email_template`, `text_template`, `customer_replied`,
    /// `file_added`, `sent_to_sign`, `signed`, `questionnaire_filled`,
    /// `template_booked`, `install_booked`, `invoice_sent`, `payment_any`,
    /// `payment_full`, `task_done` or `days_after`.
    pub trigger: String,
    /// The email or text template (`email_template`, `text_template`).
    #[serde(default)]
    pub ref_id: Option<i32>,
    /// Part of the document's or task's name (`sent_to_sign`, `signed`, `task_done`).
    #[serde(default)]
    pub text: Option<String>,
    /// Days after the step before (`days_after`).
    #[serde(default)]
    pub days: Option<i32>,
}

/// A step's new rule; `null` turns it off.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ProcessStepAuto {
    /// The rule, or none.
    pub auto: Option<StepAuto>,
}

/// A process step a project has done.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct DoneStep {
    /// The project.
    pub project_id: i32,
    /// The step.
    pub step_id: i32,
    /// Who checked it off ("Auto" when its rule did).
    pub done_by: Option<String>,
    /// When (UTC ISO).
    pub done_at: String,
}

/// A new process step.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ProcessStepInput {
    /// The stage to add it to.
    pub stage: Stage,
    /// What to do.
    pub name: String,
}

/// A process step's new name.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ProcessStepName {
    /// What to do.
    pub name: String,
}

/// Who to tell when a project checks a step off.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ProcessStepNotify {
    /// Users of the company; empty tells nobody.
    pub user_ids: Vec<i32>,
    /// Also tell the sales rep assigned to the project.
    #[serde(default)]
    pub sales_rep: bool,
}

/// A stage's steps, in their new order.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ProcessStepOrder {
    /// Step ids, first to last.
    pub ids: Vec<i32>,
}

/// Whether a project has done a step.
#[derive(Debug, Deserialize, ToSchema)]
pub struct StepDone {
    /// Done, or undone.
    pub done: bool,
}

/// A note on a project.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ProjectNote {
    /// Its id.
    pub id: i32,
    /// The text.
    pub content: String,
    /// Who made it.
    pub created_by: i32,
    /// Its author's name.
    pub author: Option<String>,
    /// When it was made, UTC ISO.
    pub created_at: String,
}

/// A file on a project.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ProjectFile {
    /// Its id.
    pub id: i32,
    /// Its name.
    pub name: String,
    /// A URL the browser can open.
    pub url: String,
    /// MIME type.
    pub content_type: String,
    /// Who made it.
    pub created_by: i32,
    /// When it was made, UTC ISO.
    pub created_at: String,
}

/// A problem the crew reported on one of the project's jobs (the job card's
/// Issues).
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ProjectIssue {
    /// Its id.
    pub id: i32,
    /// The job it was reported on; null for one reported on the project itself.
    pub event_id: Option<i32>,
    /// `templates` or `installations`; null for another calendar.
    pub kind: Option<String>,
    /// When the job is (UTC ISO); null while it waits for a date.
    pub job_date: Option<String>,
    /// What is wrong.
    pub description: String,
    /// Who reported it.
    pub author: Option<String>,
    /// When, UTC ISO.
    pub created_at: String,
    /// When it was marked solved (UTC ISO); null while it is open.
    pub solved_at: Option<String>,
    /// Who marked it solved.
    pub solved_by: Option<String>,
    /// Who reported it (user id), so they can delete one of the project's own.
    pub author_id: Option<i32>,
    /// Its pictures (URLs the browser can open).
    #[sqlx(skip)]
    pub photos: Vec<String>,
}

/// How many open issues a project has (its jobs' and its own).
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ProjectIssueCount {
    /// The project.
    pub project_id: i32,
    /// Its open issues.
    pub count: i64,
}

/// A job photo the crew took on one of the project's appointments (the job
/// card's Job photos, which the customer also sees on their page).
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct ProjectJobPhoto {
    /// Its id.
    pub id: i32,
    /// The appointment it was taken on.
    pub event_id: i32,
    /// `templates` or `installations`; null for another calendar.
    pub kind: Option<String>,
    /// A URL the browser can open.
    pub url: String,
    /// Who took it.
    pub author: Option<String>,
    /// When it was added, UTC ISO.
    pub created_at: String,
}

/// A line of a project's history.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct StageChange {
    /// Its id.
    pub id: i32,
    /// The stage before; null for the first line.
    pub from_stage: Option<String>,
    /// The stage after.
    pub to_stage: String,
    /// What did it: `deal-handoff`, `stage-change`, `fabricated`, `template-completed`, `install-completed`, ...
    pub source_action: String,
    /// Why, for a correction or cancel.
    pub reason: Option<String>,
    /// Its author's name.
    pub author: Option<String>,
    /// When it was made, UTC ISO.
    pub created_at: String,
}

/// A transaction of the customer the project may link to.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct LinkableSale {
    /// Its id.
    pub id: i32,
    /// The transaction price.
    pub price: Option<f64>,
    /// The job address.
    pub project_address: Option<String>,
    /// When it was installed, UTC ISO.
    pub installed_date: Option<String>,
    /// When it was paid, UTC ISO.
    pub paid_date: Option<String>,
}

/// A calendar its appointments go on.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct Calendar {
    /// Its id.
    pub id: i32,
    /// `templates` or `installations`.
    pub slug: String,
    /// Its name.
    pub name: String,
}

/// How long a task is usually given: the average of its earlier uses.
#[derive(Debug, Serialize, ToSchema)]
pub struct TaskTime {
    /// The task's name, as last written.
    pub name: String,
    /// Average minutes from giving it to its due date.
    pub minutes: i64,
    /// Whether its last use had a time of day, not just a date.
    pub has_time: bool,
}

/// Body of `POST /v1/projects/{id}/task-times/reset`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TaskTimeReset {
    /// The task whose remembered time to forget.
    pub name: String,
}

/// `GET /v1/projects/{id}`: a project's page.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectWorkspace {
    /// The project.
    pub project: Project,
    /// Its tasks, open and done.
    pub tasks: Vec<ProjectTask>,
    /// The company's remembered task times, to fill in the due date.
    pub task_times: Vec<TaskTime>,
    /// Its notes, newest first.
    pub notes: Vec<ProjectNote>,
    /// Its files.
    pub files: Vec<ProjectFile>,
    /// The crew's job photos from its appointments, newest first.
    pub job_photos: Vec<ProjectJobPhoto>,
    /// The problems the crew reported on its jobs, newest first.
    pub issues: Vec<ProjectIssue>,
    /// What happened, newest first.
    pub history: Vec<StageChange>,
    /// Transactions to link (empty once one is linked).
    pub sales: Vec<LinkableSale>,
    /// Everyone in the company, to give tasks to.
    pub people: Vec<Person>,
    /// The Templates and Installations calendars.
    pub calendars: Vec<Calendar>,
    /// Every appointment of the project, newest first.
    pub appointments: Vec<ProjectAppointment>,
    /// The company's own steps in the process.
    pub process_steps: Vec<ProcessStep>,
    /// Which of those steps this project has done.
    pub done_steps: Vec<DoneStep>,
    /// What the caller may do.
    pub permissions: Permissions,
    /// The caller's user id (to show which tasks and notes are theirs).
    pub user_id: i32,
}

/// Body of `POST /v1/projects/reorder`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ReorderProjects {
    /// One column's projects, in their new order.
    pub ids: Vec<i32>,
}

/// Body of `POST /v1/projects/{id}/actions`. `version` is the project's as
/// the page has it: a stale page gets a 409.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(tag = "intent", rename_all = "kebab-case")]
pub enum ProjectAction {
    /// Move forward (production).
    Stage {
        /// The project's version as the page has it; a stale page gets a 409.
        version: i32,
        /// The stage.
        stage: Stage,
    },
    /// Link the customer's transaction.
    LinkSale {
        /// The project's version as the page has it; a stale page gets a 409.
        version: i32,
        #[serde(rename = "saleId")]
        /// The transaction.
        sale_id: i32,
    },
    /// Confirm full payment on this date (ISO).
    Paid {
        /// The project's version as the page has it; a stale page gets a 409.
        version: i32,
        /// The date, ISO.
        date: String,
    },
    /// Pick the owner (null: nobody).
    Assign {
        /// The project's version as the page has it; a stale page gets a 409.
        version: i32,
        #[serde(rename = "ownerId")]
        /// The owner; null for nobody.
        owner_id: Option<i32>,
    },
    /// Put on hold or take off hold (managers).
    Hold {
        /// The project's version as the page has it; a stale page gets a 409.
        version: i32,
        #[serde(rename = "onHold")]
        /// On hold (true) or not.
        on_hold: bool,
    },
    /// Cancel, with a reason (managers).
    Cancel {
        /// The project's version as the page has it; a stale page gets a 409.
        version: i32,
        /// Why, for a correction or cancel.
        reason: String,
    },
    /// Archive a finished or cancelled project, or bring it back (managers).
    Archive {
        /// The project's version as the page has it; a stale page gets a 409.
        version: i32,
        /// Archived (true) or back on the board.
        archived: bool,
    },
    /// Change or clear the payment date, with a reason.
    CorrectPayment {
        /// The project's version as the page has it; a stale page gets a 409.
        version: i32,
        /// The date, ISO.
        date: Option<String>,
        /// Why, for a correction or cancel.
        reason: String,
    },
    /// The shop finished fabricating it (production).
    Fabricated {
        /// The project's version as the page has it; a stale page gets a 409.
        version: i32,
    },
    /// Move back, with a reason (managers).
    Correct {
        /// The project's version as the page has it; a stale page gets a 409.
        version: i32,
        /// The stage.
        stage: Stage,
        /// Why, for a correction or cancel.
        reason: String,
    },
}

impl ProjectAction {
    pub const fn version(&self) -> i32 {
        match self {
            Self::Stage { version, .. }
            | Self::LinkSale { version, .. }
            | Self::Paid { version, .. }
            | Self::Assign { version, .. }
            | Self::Hold { version, .. }
            | Self::Cancel { version, .. }
            | Self::Archive { version, .. }
            | Self::CorrectPayment { version, .. }
            | Self::Fabricated { version }
            | Self::Correct { version, .. } => *version,
        }
    }
}

/// `GET /v1/deals/{id}/project`.
#[derive(Debug, Serialize, ToSchema)]
pub struct DealProject {
    /// The deal's project, if it has one.
    pub project_id: Option<i32>,
    /// Projects are on for the company and the caller may create one.
    pub can_convert: bool,
}

/// `POST /v1/deals/{id}/project`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConvertedDeal {
    /// The project it belongs to.
    pub project_id: i32,
    /// False when the deal had a project already.
    pub created: bool,
}

/// Body of `POST /v1/projects/{id}/tasks`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TaskInput {
    /// 1 to 255 characters.
    pub name: String,
    /// Who does it: anyone in the company; null for nobody.
    #[serde(rename = "assignedUserId", default)]
    pub assigned_user_id: Option<i32>,
    /// `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM[:SS]` store wall time.
    #[serde(default)]
    pub deadline: Option<String>,
    /// `low`, `medium` (default) or `high`.
    #[serde(default)]
    pub priority: Option<String>,
}

/// Body of `PATCH /v1/projects/{id}/tasks/{task_id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TaskUpdate {
    /// Done (true) or open again (false).
    pub completed: bool,
}

/// Body of `POST /v1/projects/{id}/notes`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NoteInput {
    /// The text.
    pub content: String,
}

/// One file uploaded with `POST /v1/uploads` (folder `deals`).
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewProjectFile {
    /// Where it was uploaded.
    pub url: String,
    /// Its name.
    pub name: String,
    #[serde(rename = "contentType", default)]
    /// MIME type.
    pub content_type: Option<String>,
}

/// Body of `POST /v1/projects/{id}/files`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewProjectFiles {
    /// Its files.
    pub files: Vec<NewProjectFile>,
}
