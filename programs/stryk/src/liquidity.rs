use crate::{state::*, StrykError};
use quasar_lang::{cpi::Seed, prelude::*};
use quasar_spl::prelude::*;

#[derive(Accounts)]
pub struct MoveInventory {
    pub maker: Signer,
    pub market: Account<Market>,
    #[account(has_one(market), has_one(maker))]
    pub lane: Account<MakerLane>,
    pub mint: Account<Mint>,
    #[account(mut, associated_token(mint = mint, authority = maker, token_program = token_program))]
    pub wallet: Account<Token>,
    #[account(mut, associated_token(mint = mint, authority = lane, token_program = token_program))]
    pub inventory: Account<Token>,
    pub token_program: Program<TokenProgram>,
}

impl MoveInventory {
    fn validate(&self, amount: u64) -> Result<(), ProgramError> {
        let mint = self.mint.address();
        require!(
            amount > 0 && (mint == &self.market.base_mint || mint == &self.market.quote_mint),
            StrykError::InvalidTerms
        );
        Ok(())
    }

    pub fn deposit(&self, amount: u64) -> Result<(), ProgramError> {
        self.validate(amount)?;
        self.token_program
            .transfer_checked(
                &self.wallet,
                &self.mint,
                &self.inventory,
                &self.maker,
                amount,
                self.mint.decimals(),
            )
            .invoke()
    }

    pub fn withdraw(&self, amount: u64) -> Result<(), ProgramError> {
        self.validate(amount)?;
        let lane_id = self.lane.lane_id.get().to_le_bytes();
        let bump = [self.lane.bump];
        let seeds = lane_seeds(self.market.address(), self.maker.address(), &lane_id, &bump);
        self.token_program
            .transfer_checked(
                &self.inventory,
                &self.mint,
                &self.wallet,
                &self.lane,
                amount,
                self.mint.decimals(),
            )
            .invoke_signed(&seeds)
    }
}

pub(crate) fn lane_seeds<'a>(
    market: &'a Address,
    maker: &'a Address,
    lane_id: &'a [u8; 2],
    bump: &'a [u8; 1],
) -> [Seed<'a>; 5] {
    [
        Seed::from(b"maker" as &[u8]),
        Seed::from(market.as_ref()),
        Seed::from(maker.as_ref()),
        Seed::from(lane_id.as_slice()),
        Seed::from(bump.as_slice()),
    ]
}
