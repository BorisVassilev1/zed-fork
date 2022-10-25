use anyhow::Result;
use rusqlite::{params, Connection};

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::pane::{PaneGroupId, PaneId, SerializedPane, SerializedPaneGroup};

use super::Db;

// TODO for workspace serialization:
// - Update return types to unwrap all of the results into dummy values
// - On database failure to initialize, delete the DB file
// - Update paths to be blobs ( :( https://users.rust-lang.org/t/how-to-safely-store-a-path-osstring-in-a-sqllite-database/79712/10 )
// - Convert hot paths to prepare-cache-execute style

pub(crate) const WORKSPACE_M_1: &str = "
CREATE TABLE workspaces(
    workspace_id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT DEFAULT CURRENT_TIMESTAMP
) STRICT;

CREATE TABLE worktree_roots(
    worktree_root TEXT NOT NULL,
    workspace_id INTEGER NOT NULL,
    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id) ON DELETE CASCADE
    PRIMARY KEY(worktree_root, workspace_id)
) STRICT;
";

// Zed stores items with ids which are a combination of a view id during a given run and a workspace id. This

//      Case 1: Starting Zed Contextless
//          > Zed -> Reopen the last
//      Case 2: Starting Zed with a project folder
//          > Zed ~/projects/Zed
//      Case 3: Starting Zed with a file
//          > Zed ~/projects/Zed/cargo.toml
//      Case 4: Starting Zed with multiple project folders
//          > Zed ~/projects/Zed ~/projects/Zed.dev

#[derive(Debug, PartialEq, Eq, Copy, Clone, Default)]
pub struct WorkspaceId(i64);

struct WorkspaceRow {
    pub center_group_id: PaneGroupId,
    pub dock_pane_id: PaneId,
}

#[derive(Default, Debug)]
pub struct SerializedWorkspace {
    pub workspace_id: WorkspaceId,
    // pub center_group: SerializedPaneGroup,
    // pub dock_pane: Option<SerializedPane>,
}

impl Db {
    /// Finds or creates a workspace id for the given set of worktree roots. If the passed worktree roots is empty, return the
    /// the last workspace id
    pub fn workspace_for_worktree_roots(
        &self,
        worktree_roots: &[Arc<Path>],
    ) -> SerializedWorkspace {
        // Find the workspace id which is uniquely identified by this set of paths return it if found
        if let Ok(Some(workspace_id)) = self.workspace_id(worktree_roots) {
            // TODO
            // let workspace_row = self.get_workspace_row(workspace_id);
            // let center_group = self.get_pane_group(workspace_row.center_group_id);
            // let dock_pane = self.get_pane(workspace_row.dock_pane_id);

            SerializedWorkspace {
                workspace_id,
                // center_group,
                // dock_pane: Some(dock_pane),
            }
        } else {
            self.make_new_workspace()
        }
    }

    fn make_new_workspace(&self) -> SerializedWorkspace {
        self.real()
            .map(|db| {
                let lock = db.connection.lock();
                match lock.execute("INSERT INTO workspaces DEFAULT VALUES;", []) {
                    Ok(_) => SerializedWorkspace {
                        workspace_id: WorkspaceId(lock.last_insert_rowid()),
                    },
                    Err(_) => Default::default(),
                }
            })
            .unwrap_or_default()
    }

    fn workspace_id<P>(&self, worktree_roots: &[P]) -> Result<Option<WorkspaceId>>
    where
        P: AsRef<Path>,
    {
        self.real()
            .map(|db| {
                let lock = db.connection.lock();

                get_workspace_id(worktree_roots, &lock)
            })
            .unwrap_or(Ok(None))
    }

    // fn get_workspace_row(&self, workspace_id: WorkspaceId) -> WorkspaceRow {
    //     unimplemented!()
    // }

    /// Updates the open paths for the given workspace id. Will garbage collect items from
    /// any workspace ids which are no replaced by the new workspace id. Updates the timestamps
    /// in the workspace id table
    pub fn update_worktree_roots<P>(
        &self,
        workspace_id: &WorkspaceId,
        worktree_roots: &[P],
    ) -> Result<()>
    where
        P: AsRef<Path>,
    {
        self.real()
            .map(|db| {
                let mut lock = db.connection.lock();

                let tx = lock.transaction()?;

                {
                    // Lookup any old WorkspaceIds which have the same set of roots, and delete them.
                    let preexisting_id = get_workspace_id(worktree_roots, &tx)?;
                    if let Some(preexisting_id) = preexisting_id {
                        if preexisting_id != *workspace_id {
                            // Should also delete fields in other tables
                            tx.execute(
                                "DELETE FROM workspaces WHERE workspace_id = ?",
                                [preexisting_id.0],
                            )?;
                        }
                    }

                    tx.execute(
                        "DELETE FROM worktree_roots WHERE workspace_id = ?",
                        [workspace_id.0],
                    )?;

                    for root in worktree_roots {
                        // TODO: Update this to use blobs
                        let path = root.as_ref().to_string_lossy().to_string();

                        let mut stmt = tx.prepare_cached("INSERT INTO worktree_roots(workspace_id, worktree_root) VALUES (?, ?)")?;
                        stmt.execute(params![workspace_id.0, path])?;
                    }

                    let mut stmt = tx.prepare_cached("UPDATE workspaces SET timestamp = CURRENT_TIMESTAMP WHERE workspace_id = ?")?;
                    stmt.execute([workspace_id.0])?;
                }
                tx.commit()?;

                Ok(())
            })
            .unwrap_or(Ok(()))
    }

    /// Returns the previous workspace ids sorted by last modified along with their opened worktree roots
    pub fn recent_workspaces(&self, limit: usize) -> Result<Vec<(WorkspaceId, Vec<Arc<Path>>)>> {
        // Return all the workspace ids and their associated paths ordered by the access timestamp
        //ORDER BY timestamps
        self.real()
            .map(|db| {
                let mut lock = db.connection.lock();

                let tx = lock.transaction()?;
                let result = {
                    let mut stmt = tx.prepare_cached(
                        "SELECT workspace_id FROM workspaces ORDER BY timestamp DESC LIMIT ?",
                    )?;
                    let workspace_ids = stmt
                        .query_map([limit], |row| Ok(WorkspaceId(row.get(0)?)))?
                        .collect::<Result<Vec<_>, rusqlite::Error>>()?;

                    let mut result = Vec::new();
                    let mut stmt = tx.prepare_cached(
                        "SELECT worktree_root FROM worktree_roots WHERE workspace_id = ?",
                    )?;
                    for workspace_id in workspace_ids {
                        let roots = stmt
                            .query_map([workspace_id.0], |row| {
                                let row = row.get::<_, String>(0)?;
                                Ok(PathBuf::from(Path::new(&row)).into())
                            })?
                            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
                        result.push((workspace_id, roots))
                    }

                    result
                };

                tx.commit()?;

                return Ok(result);
            })
            .unwrap_or_else(|| Ok(Vec::new()))
    }
}

fn get_workspace_id<P>(
    worktree_roots: &[P],
    connection: &Connection,
) -> Result<Option<WorkspaceId>, anyhow::Error>
where
    P: AsRef<Path>,
{
    // Prepare the array binding string. SQL doesn't have syntax for this, so
    // we have to do it ourselves.
    let mut array_binding_stmt = "(".to_string();
    for i in 0..worktree_roots.len() {
        array_binding_stmt.push_str(&format!("?{}", (i + 1))); //sqlite is 1-based
        if i < worktree_roots.len() - 1 {
            array_binding_stmt.push(',');
            array_binding_stmt.push(' ');
        }
    }
    array_binding_stmt.push(')');
    // Any workspace can have multiple independent paths, and these paths
    // can overlap in the database. Take this test data for example:
    //
    // [/tmp, /tmp2] -> 1
    // [/tmp] -> 2
    // [/tmp2, /tmp3] -> 3
    //
    // This would be stred in the database like so:
    //
    // ID PATH
    // 1  /tmp
    // 1  /tmp2
    // 2  /tmp
    // 3  /tmp2
    // 3  /tmp3
    //
    // Note how both /tmp and /tmp2 are associated with multiple workspace IDs.
    // So, given an array of worktree roots, how can we find the exactly matching ID?
    // Let's analyze what happens when querying for [/tmp, /tmp2], from the inside out:
    //  - We start with a join of this table on itself, generating every possible
    //    pair of ((path, ID), (path, ID)), and filtering the join down to just the
    //    *overlapping* workspace IDs. For this small data set, this would look like:
    //
    //    wt1.ID wt1.PATH | wt2.ID wt2.PATH
    //    3      /tmp3      3      /tmp2
    //
    //  - Moving one SELECT out, we use the first pair's ID column to invert the selection,
    //    meaning we now have a list of all the entries for our array and *subsets*
    //    of our array:
    //
    //    ID PATH
    //    1  /tmp
    //    2  /tmp
    //    2  /tmp2
    //
    // - To trim out the subsets, we need to exploit the fact that there can be no duplicate
    //   entries in this table. We can just use GROUP BY, COUNT, and a WHERE clause that checks
    //   for the length of our array:
    //
    //    ID num_matching
    //    1  2
    //
    // And we're done! We've found the matching ID correctly :D
    // However, due to limitations in sqlite's query binding, we still have to do some string
    // substitution to generate the correct query
    let query = format!(
        r#"
                    SELECT workspace_id 
                    FROM (SELECT count(workspace_id) as num_matching, workspace_id FROM worktree_roots
                          WHERE worktree_root in {array_bind} AND workspace_id NOT IN
                            (SELECT wt1.workspace_id FROM worktree_roots as wt1
                             JOIN worktree_roots as wt2
                             ON wt1.workspace_id = wt2.workspace_id
                             WHERE wt1.worktree_root NOT in {array_bind} AND wt2.worktree_root in {array_bind})
                          GROUP BY workspace_id)
                   WHERE num_matching = ?
                "#,
        array_bind = array_binding_stmt
    );
    let mut stmt = connection.prepare_cached(&query)?;
    // Make sure we bound the parameters correctly
    debug_assert!(worktree_roots.len() + 1 == stmt.parameter_count());

    for i in 0..worktree_roots.len() {
        // TODO: Update this to use blobs
        let path = &worktree_roots[i].as_ref().to_string_lossy().to_string();
        stmt.raw_bind_parameter(i + 1, path)?
    }
    // No -1, because SQLite is 1 based
    stmt.raw_bind_parameter(worktree_roots.len() + 1, worktree_roots.len())?;

    let mut rows = stmt.raw_query();
    if let Ok(Some(row)) = rows.next() {
        return Ok(Some(WorkspaceId(row.get(0)?)));
    }
    // Ensure that this query only returns one row. The PRIMARY KEY constraint should catch this case
    // but this is here to catch it if someone refactors that constraint out.
    debug_assert!(matches!(rows.next(), Ok(None)));
    Ok(None)
}

#[cfg(test)]
mod tests {

    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        thread::sleep,
        time::Duration,
    };

    use crate::Db;

    use super::WorkspaceId;

    #[test]
    fn test_more_workspace_ids() {
        let data = &[
            (WorkspaceId(1), vec!["/tmp1"]),
            (WorkspaceId(2), vec!["/tmp1", "/tmp2"]),
            (WorkspaceId(3), vec!["/tmp1", "/tmp2", "/tmp3"]),
            (WorkspaceId(4), vec!["/tmp2", "/tmp3"]),
            (WorkspaceId(5), vec!["/tmp2", "/tmp3", "/tmp4"]),
            (WorkspaceId(6), vec!["/tmp2", "/tmp4"]),
            (WorkspaceId(7), vec!["/tmp2"]),
        ];

        let db = Db::open_in_memory();

        for (workspace_id, entries) in data {
            db.make_new_workspace();
            db.update_worktree_roots(workspace_id, entries).unwrap();
        }

        assert_eq!(Some(WorkspaceId(1)), db.workspace_id(&["/tmp1"]).unwrap());
        assert_eq!(
            db.workspace_id(&["/tmp1", "/tmp2"]).unwrap(),
            Some(WorkspaceId(2))
        );
        assert_eq!(
            db.workspace_id(&["/tmp1", "/tmp2", "/tmp3"]).unwrap(),
            Some(WorkspaceId(3))
        );
        assert_eq!(
            db.workspace_id(&["/tmp2", "/tmp3"]).unwrap(),
            Some(WorkspaceId(4))
        );
        assert_eq!(
            db.workspace_id(&["/tmp2", "/tmp3", "/tmp4"]).unwrap(),
            Some(WorkspaceId(5))
        );
        assert_eq!(
            db.workspace_id(&["/tmp2", "/tmp4"]).unwrap(),
            Some(WorkspaceId(6))
        );
        assert_eq!(db.workspace_id(&["/tmp2"]).unwrap(), Some(WorkspaceId(7)));

        assert_eq!(db.workspace_id(&["/tmp1", "/tmp5"]).unwrap(), None);
        assert_eq!(db.workspace_id(&["/tmp5"]).unwrap(), None);
        assert_eq!(
            db.workspace_id(&["/tmp2", "/tmp3", "/tmp4", "/tmp5"])
                .unwrap(),
            None
        );
    }

    #[test]
    fn test_detect_workspace_id() {
        let data = &[
            (WorkspaceId(1), vec!["/tmp"]),
            (WorkspaceId(2), vec!["/tmp", "/tmp2"]),
            (WorkspaceId(3), vec!["/tmp", "/tmp2", "/tmp3"]),
        ];

        let db = Db::open_in_memory();

        for (workspace_id, entries) in data {
            db.make_new_workspace();
            db.update_worktree_roots(workspace_id, entries).unwrap();
        }

        assert_eq!(db.workspace_id(&["/tmp2"]).unwrap(), None);
        assert_eq!(db.workspace_id(&["/tmp2", "/tmp3"]).unwrap(), None);
        assert_eq!(db.workspace_id(&["/tmp"]).unwrap(), Some(WorkspaceId(1)));
        assert_eq!(
            db.workspace_id(&["/tmp", "/tmp2"]).unwrap(),
            Some(WorkspaceId(2))
        );
        assert_eq!(
            db.workspace_id(&["/tmp", "/tmp2", "/tmp3"]).unwrap(),
            Some(WorkspaceId(3))
        );
    }

    fn arc_path(path: &'static str) -> Arc<Path> {
        PathBuf::from(path).into()
    }

    #[test]
    fn test_tricky_overlapping_updates() {
        // DB state:
        // (/tree) -> ID: 1
        // (/tree, /tree2) -> ID: 2
        // (/tree2, /tree3) -> ID: 3

        // -> User updates 2 to: (/tree2, /tree3)

        // DB state:
        // (/tree) -> ID: 1
        // (/tree2, /tree3) -> ID: 2
        // Get rid of 3 for garbage collection

        let data = &[
            (WorkspaceId(1), vec!["/tmp"]),
            (WorkspaceId(2), vec!["/tmp", "/tmp2"]),
            (WorkspaceId(3), vec!["/tmp2", "/tmp3"]),
        ];

        let db = Db::open_in_memory();

        // Load in the test data
        for (workspace_id, entries) in data {
            db.workspace_for_worktree_roots(&[]);
            db.update_worktree_roots(workspace_id, entries).unwrap();
        }

        // Make sure the timestamp updates
        sleep(Duration::from_secs(1));
        // Execute the update
        db.update_worktree_roots(&WorkspaceId(2), &["/tmp2", "/tmp3"])
            .unwrap();

        // Make sure that workspace 3 doesn't exist
        assert_eq!(
            db.workspace_id(&["/tmp2", "/tmp3"]).unwrap(),
            Some(WorkspaceId(2))
        );

        // And that workspace 1 was untouched
        assert_eq!(db.workspace_id(&["/tmp"]).unwrap(), Some(WorkspaceId(1)));

        // And that workspace 2 is no longer registered under this
        assert_eq!(db.workspace_id(&["/tmp", "/tmp2"]).unwrap(), None);

        let recent_workspaces = db.recent_workspaces(10).unwrap();
        assert_eq!(
            recent_workspaces.get(0).unwrap(),
            &(WorkspaceId(2), vec![arc_path("/tmp2"), arc_path("/tmp3")])
        );
        assert_eq!(
            recent_workspaces.get(1).unwrap(),
            &(WorkspaceId(1), vec![arc_path("/tmp")])
        );
    }
}
