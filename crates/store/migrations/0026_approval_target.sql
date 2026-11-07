-- What a proposal acts on, so a newer equivalent proposal can supersede an open one.
ALTER TABLE approvals ADD COLUMN target text;
CREATE INDEX approvals_open_target ON approvals (tenant_id, tool, target) WHERE status = 'pending';
