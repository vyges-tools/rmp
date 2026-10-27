// SPDX-License-Identifier: Apache-2.0
//! The design database as the stages read and edit it.

use vyges_opendb::Db;

use crate::cut::blif::{BlifDesign, ITerm};

/// The database's top block.
pub struct OdbBlif<'a> {
    pub db: &'a mut Db,
}

fn some(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

impl BlifDesign for OdbBlif<'_> {
    fn inst_id(&self, inst: &str) -> u32 {
        self.db.inst_id(inst).unwrap_or(u32::MAX)
    }
    fn inst_master(&self, inst: &str) -> String {
        self.db.inst_master(inst)
    }
    fn inst_iterms(&self, inst: &str) -> Vec<ITerm> {
        self.db
            .iterm_names(inst)
            .into_iter()
            .map(|m| ITerm {
                sig_type: self.db.iterm_get_sig_type(inst, &m),
                io_type: self.db.iterm_get_io_type(inst, &m),
                net: some(self.db.iterm_get_net(inst, &m)),
                mterm: m,
            })
            .collect()
    }
    fn net_iterms(&self, net: &str) -> Vec<(String, String)> {
        self.db.net_iterm_pairs(net).unwrap_or_default()
    }
    fn net_bterms(&self, net: &str) -> Vec<(String, String)> {
        self.db.net_bterms(net).into_iter().map(|b| {
            let io = self.db.bterm_get_io_type(&b);
            (b, io)
        }).collect()
    }
    fn master_exists(&self, master: &str) -> bool {
        self.db.master_mterm_io(master).is_ok()
    }
    fn master_mterms(&self, master: &str) -> Vec<String> {
        self.db.master_mterm_io(master).map(|v| v.into_iter().map(|(n, _)| n).collect()).unwrap_or_default()
    }
    fn net_exists(&self, net: &str) -> bool {
        !self.db.net_get_sig_type(net).is_empty()
    }
    fn inst_exists(&self, inst: &str) -> bool {
        !self.db.inst_master(inst).is_empty()
    }
    fn iterm_exists(&self, inst: &str, mterm: &str) -> bool {
        !self.db.iterm_get_io_type(inst, mterm).is_empty()
    }
    fn disconnect(&mut self, inst: &str, mterm: &str) -> Result<(), String> {
        self.db.disconnect(inst, mterm).map_err(|e| e.to_string())
    }
    fn destroy_net(&mut self, net: &str) -> Result<(), String> {
        self.db.destroy_net(net).map_err(|e| e.to_string())
    }
    fn destroy_inst(&mut self, inst: &str) -> Result<(), String> {
        self.db.destroy_inst(inst).map_err(|e| e.to_string())
    }
    fn create_net(&mut self, net: &str) -> Result<bool, String> {
        self.db.net_try_create(net).map_err(|e| e.to_string())
    }
    fn create_inst(&mut self, master: &str, name: &str) -> Result<bool, String> {
        self.db.inst_try_create(master, name).map_err(|e| e.to_string())
    }
    fn connect(&mut self, inst: &str, mterm: &str, net: &str) -> Result<(), String> {
        self.db.connect(inst, mterm, net).map_err(|e| e.to_string())
    }
}

/// What a re-synthesis asks of the design beyond the BLIF reader's questions.
pub use vyges_cut::design::CutDesign;

impl CutDesign for OdbBlif<'_> {
    fn net_id(&self, net: &str) -> u32 {
        self.db.net_id(net).unwrap_or(u32::MAX)
    }
    fn bterm_net(&self, bterm: &str) -> Option<String> {
        some(self.db.bterm_get_net(bterm))
    }
    fn inst_dont_touch(&self, inst: &str) -> bool {
        self.db.inst_is_do_not_touch(inst)
    }
    fn net_dont_touch(&self, net: &str) -> bool {
        self.db.net_is_do_not_touch(net)
    }
    fn new_net_name(&mut self, base: &str) -> Result<String, String> {
        self.db.make_new_net_name(base, "ALWAYS").map_err(|e| e.to_string())
    }
}
