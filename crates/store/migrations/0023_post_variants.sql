-- Alternative texts of a post and the comment the owner posts first.
ALTER TABLE posts ADD COLUMN variants jsonb NOT NULL DEFAULT '[]';
ALTER TABLE posts ADD COLUMN first_comment text;
