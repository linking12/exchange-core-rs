use crate::core::common::cmd::command_result_code::CommandResultCode;
use crate::core::common::cmd::order_command::OrderCommand;
use crate::core::common::user_profile::UserProfile;
use crate::core::processors::risk_engine::RiskEngine;
use crate::core::processors::symbol_specification_provider::SymbolSpecificationProvider;
use crate::core::processors::user_profile_service::UserProfileService;

pub struct TwoStepContext<'a> {
    pub risk: &'a mut RiskEngine,
    pub ups: &'a mut UserProfileService,
    pub ssp: &'a SymbolSpecificationProvider,
}

impl<'a> TwoStepContext<'a> {
    pub fn new(
        risk: &'a mut RiskEngine,
        ups: &'a mut UserProfileService,
        ssp: &'a SymbolSpecificationProvider,
    ) -> Self {
        Self { risk, ups, ssp }
    }
}

pub trait TwoStepCommandProcessor {

    fn collect(&self, ctx: &mut TwoStepContext, cmd: &mut OrderCommand) -> CommandResultCode;

    fn apply(&self, ctx: &mut TwoStepContext, cmd: &mut OrderCommand);

    /// 对满足 select 的用户并行执行 per-user 纯函数 map_user(逐用户工作单元),
    /// 按 uid 序返回每用户结果,交由调用方单线程 reduce。
    /// map_user 只拿 &UserProfile —— 结构上无法改共享状态(并行只读由类型强制)。
    fn map_users<T: Send>(
        &self,
        ctx: &TwoStepContext,
        select: impl Fn(&UserProfile) -> bool,
        map_user: impl Fn(&UserProfile) -> T + Sync,
    ) -> Vec<T> {
        let users: Vec<&UserProfile> = ctx.ups.users.values().filter(|u| select(u)).collect();
        ctx.risk.compute_pool().map(&users, |u| map_user(u))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::processors::risk_engine::RiskEngine;
    use crate::core::processors::symbol_specification_provider::SymbolSpecificationProvider;
    use crate::core::processors::user_profile_service::UserProfileService;
    use crate::core::common::cmd::command_result_code::CommandResultCode;
    use crate::core::common::cmd::order_command::OrderCommand;

    struct Dummy;
    impl TwoStepCommandProcessor for Dummy {
        fn collect(&self, _c: &mut TwoStepContext, _cmd: &mut OrderCommand) -> CommandResultCode {
            CommandResultCode::Success
        }
        fn apply(&self, _c: &mut TwoStepContext, _cmd: &mut OrderCommand) {}
    }

    #[test]
    fn map_users_returns_per_user_in_uid_order() {
        let mut risk = RiskEngine::new();
        let mut ups = UserProfileService::new();
        for uid in [3i64, 1, 2] {
            assert_eq!(ups.add_empty_user_profile(uid), CommandResultCode::Success);
        }
        let ssp = SymbolSpecificationProvider::new();
        let ctx = TwoStepContext::new(&mut risk, &mut ups, &ssp);
        let out = Dummy.map_users(&ctx, |_| true, |u| u.uid);
        assert_eq!(out, vec![1, 2, 3], "BTreeMap values() 有序 -> 结果按 uid 升序");
    }
}
