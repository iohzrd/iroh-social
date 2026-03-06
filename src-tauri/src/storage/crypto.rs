use rusqlite::params;

use super::Storage;

impl Storage {
    pub fn save_ratchet_session(
        &self,
        peer_pubkey: &str,
        state_json: &str,
        updated_at: u64,
    ) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO dm_ratchet_sessions (peer_pubkey, state_json, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(peer_pubkey) DO UPDATE SET state_json=?2, updated_at=?3",
            params![peer_pubkey, state_json, updated_at as i64],
        )?;
        Ok(())
    }

    pub fn get_ratchet_session(&self, peer_pubkey: &str) -> anyhow::Result<Option<String>> {
        let db = self.db.lock().unwrap();
        let mut stmt =
            db.prepare("SELECT state_json FROM dm_ratchet_sessions WHERE peer_pubkey=?1")?;
        let mut rows = stmt.query(params![peer_pubkey])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }
}
