-- Raptor jobs from the quote's buttons (sale companies): Mark as Sold queues
-- the project (`waiting`) and Undo sale marks its jobs for deletion
-- (`removing`); the next raptor-jobs run makes or deletes them.
ALTER TABLE project_raptor_jobs
    MODIFY status ENUM('working', 'done', 'failed', 'waiting', 'removing') NOT NULL DEFAULT 'working';
