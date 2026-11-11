-- The full-text index of the task search (`q` of `GET /v1/tasks`): one document for each
-- task, with its id, title, own text and the text of its notes. Words are stemmed
-- (porter), so `deploy` finds `deployed`. Triggers keep it equal to the tables on every
-- write, whichever statement makes it.

-- A stable integer for each task: the rowid of its document.
CREATE TABLE task_search_docs (
    doc INTEGER PRIMARY KEY,
    task TEXT NOT NULL UNIQUE
) STRICT;

CREATE VIRTUAL TABLE task_search USING fts5 (
    id, title, body, notes,
    tokenize = 'porter unicode61'
);

INSERT INTO task_search_docs (task) SELECT id FROM tasks ORDER BY rowid;
INSERT INTO task_search (rowid, id, title, body, notes)
SELECT d.doc, t.id, t.title,
       concat_ws(char(10), t.body, t.acceptance, t.summary, t.blocked_reason),
       coalesce((SELECT group_concat(n.text, char(10)) FROM notes n WHERE n.task = t.id), '')
FROM tasks t JOIN task_search_docs d ON d.task = t.id;

CREATE TRIGGER task_search_task_insert AFTER INSERT ON tasks BEGIN
    INSERT INTO task_search_docs (task) VALUES (new.id);
    INSERT INTO task_search (rowid, id, title, body, notes)
    VALUES ((SELECT doc FROM task_search_docs WHERE task = new.id), new.id, new.title,
            concat_ws(char(10), new.body, new.acceptance, new.summary, new.blocked_reason),
            '');
END;

CREATE TRIGGER task_search_task_update
AFTER UPDATE OF title, body, acceptance, summary, blocked_reason ON tasks
WHEN old.title IS NOT new.title OR old.body IS NOT new.body
    OR old.acceptance IS NOT new.acceptance OR old.summary IS NOT new.summary
    OR old.blocked_reason IS NOT new.blocked_reason
BEGIN
    UPDATE task_search
    SET title = new.title,
        body = concat_ws(char(10), new.body, new.acceptance, new.summary, new.blocked_reason)
    WHERE rowid = (SELECT doc FROM task_search_docs WHERE task = new.id);
END;

CREATE TRIGGER task_search_task_delete AFTER DELETE ON tasks BEGIN
    DELETE FROM task_search
    WHERE rowid = (SELECT doc FROM task_search_docs WHERE task = old.id);
    DELETE FROM task_search_docs WHERE task = old.id;
END;

CREATE TRIGGER task_search_note_insert AFTER INSERT ON notes BEGIN
    UPDATE task_search
    SET notes = coalesce(
        (SELECT group_concat(n.text, char(10)) FROM notes n WHERE n.task = new.task), '')
    WHERE rowid = (SELECT doc FROM task_search_docs WHERE task = new.task);
END;

CREATE TRIGGER task_search_note_update AFTER UPDATE OF text, task ON notes BEGIN
    UPDATE task_search
    SET notes = coalesce(
        (SELECT group_concat(n.text, char(10)) FROM notes n WHERE n.task = old.task), '')
    WHERE rowid = (SELECT doc FROM task_search_docs WHERE task = old.task);
    UPDATE task_search
    SET notes = coalesce(
        (SELECT group_concat(n.text, char(10)) FROM notes n WHERE n.task = new.task), '')
    WHERE rowid = (SELECT doc FROM task_search_docs WHERE task = new.task);
END;

CREATE TRIGGER task_search_note_delete AFTER DELETE ON notes BEGIN
    UPDATE task_search
    SET notes = coalesce(
        (SELECT group_concat(n.text, char(10)) FROM notes n WHERE n.task = old.task), '')
    WHERE rowid = (SELECT doc FROM task_search_docs WHERE task = old.task);
END;
