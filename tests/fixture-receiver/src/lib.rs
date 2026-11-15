#![no_std]

use quasar_lang::prelude::*;

declare_id!("B7s3eyFtLSijY7bAx99kREdfpMK55MCr5r77j837TmCs");

#[program]
mod fixture_receiver {
    use super::*;

    #[instruction(discriminator = 1)]
    pub fn write(ctx: Ctx<Write>, data: [u8; 134]) -> Result<(), ProgramError> {
        let account = ctx.accounts.update.to_account_view();
        if !account.owned_by(&ID) || account.data_len() != data.len() {
            return Err(ProgramError::InvalidAccountData);
        }
        // Ownership and signer checks make the deliberately synthetic payload localnet-only.
        let mut view = account.clone();
        view.try_borrow_mut()?.copy_from_slice(&data);
        Ok(())
    }
}

#[derive(Accounts)]
struct Write {
    #[account(mut)]
    update: Signer,
}
