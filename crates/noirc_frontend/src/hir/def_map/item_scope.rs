use super::{namespace::PerNs, ModuleDefId, ModuleId};
use crate::{
    node_interner::{Definition, FuncId, NodeInterner, StmtId, StructId},
    Ident,
};
use std::collections::{hash_map::Entry, HashMap};

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Visibility {
    Public,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ItemScope {
    types: HashMap<Ident, (ModuleDefId, Visibility)>,
    values: HashMap<Ident, (ModuleDefId, Visibility)>,

    defs: Vec<ModuleDefId>,
}

impl ItemScope {
    pub fn add_definition(
        &mut self,
        name: Ident,
        mod_def: ModuleDefId,
    ) -> Result<(), (Ident, Ident)> {
        self.add_item_to_namespace(name, mod_def)?;
        self.defs.push(mod_def);
        Ok(())
    }

    /// Returns an Err if there is already an item
    /// in the namespace with that exact name.
    /// The Err will return (old_item, new_item)
    pub fn add_item_to_namespace(
        &mut self,
        name: Ident,
        mod_def: ModuleDefId,
    ) -> Result<(), (Ident, Ident)> {
        let add_item = |map: &mut HashMap<Ident, (ModuleDefId, Visibility)>| {
            if let Entry::Occupied(o) = map.entry(name.clone()) {
                let old_ident = o.key();
                Err((old_ident.clone(), name))
            } else {
                map.insert(name, (mod_def, Visibility::Public));
                Ok(())
            }
        };

        match mod_def {
            ModuleDefId::ModuleId(_) => add_item(&mut self.types),
            ModuleDefId::VariableId(_) => add_item(&mut self.values),
            ModuleDefId::TypeId(_) => add_item(&mut self.types),
            ModuleDefId::GlobalId(_) => add_item(&mut self.values),
        }
    }

    pub fn define_module_def(
        &mut self,
        name: Ident,
        mod_id: ModuleId,
    ) -> Result<(), (Ident, Ident)> {
        self.add_definition(name, mod_id.into())
    }

    pub fn define_func_def(
        &mut self,
        name: Ident,
        local_id: FuncId,
        interner: &mut NodeInterner,
    ) -> Result<(), (Ident, Ident)> {
        let id = interner.push_function_definition(name.to_string(), local_id);
        self.add_definition(name, ModuleDefId::VariableId(id))
    }

    pub fn define_struct_def(
        &mut self,
        name: Ident,
        local_id: StructId,
    ) -> Result<(), (Ident, Ident)> {
        self.add_definition(name, ModuleDefId::TypeId(local_id))
    }

    pub fn define_global(&mut self, name: Ident, stmt_id: StmtId) -> Result<(), (Ident, Ident)> {
        self.add_definition(name, ModuleDefId::GlobalId(stmt_id))
    }

    pub fn find_module_with_name(&self, mod_name: &Ident) -> Option<&ModuleId> {
        let (module_def, _) = self.types.get(mod_name)?;
        match module_def {
            ModuleDefId::ModuleId(id) => Some(id),
            _ => None,
        }
    }

    pub fn find_func_with_name(
        &self,
        func_name: &Ident,
        interner: &NodeInterner,
    ) -> Option<FuncId> {
        let (module_def, _) = self.values.get(func_name)?;
        match module_def {
            ModuleDefId::VariableId(id) => match interner.definition(*id).definition {
                Definition::Function(id) => Some(id),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn find_name(&self, name: &Ident) -> PerNs {
        PerNs { types: self.types.get(name).cloned(), values: self.values.get(name).cloned() }
    }

    pub fn definitions(&self) -> Vec<ModuleDefId> {
        self.defs.clone()
    }
    pub fn types(&self) -> &HashMap<Ident, (ModuleDefId, Visibility)> {
        &self.types
    }
    pub fn values(&self) -> &HashMap<Ident, (ModuleDefId, Visibility)> {
        &self.values
    }
}
