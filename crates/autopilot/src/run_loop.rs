use {
    crate::{
        database::competition::Competition,
        domain::{
            self,
            OrderUid,
            auction::Id,
            competition::{
                self,
                Solution,
                SolutionError,
                SolverParticipationGuard,
                TradedOrder,
                Unranked,
            },
            eth::{self, TxId},
            settlement::{ExecutionEnded, ExecutionStarted},
        },
        infra::{
            self,
            solvers::dto::{settle, solve},
        },
        maintenance::Maintenance,
        run::Liveness,
        solvable_orders::SolvableOrdersCache,
    },
    ::observe::metrics,
    anyhow::Result,
    database::order_events::OrderEventLabel,
    ethcontract::U256,
    ethrpc::block_stream::BlockInfo,
    futures::{FutureExt, TryFutureExt},
    itertools::Itertools,
    model::solver_competition::{
        CompetitionAuction,
        Order,
        Score,
        SolverCompetitionDB,
        SolverSettlement,
    },
    primitive_types::H256,
    rand::seq::SliceRandom,
    shared::token_list::AutoUpdatingTokenList,
    std::{
        collections::{HashMap, HashSet},
        sync::Arc,
        time::{Duration, Instant},
    },
    tokio::sync::Mutex,
    tracing::Instrument,
};

pub struct Config {
    pub submission_deadline: u64,
    pub max_settlement_transaction_wait: Duration,
    pub solve_deadline: Duration,
    /// How much time past observing the current block the runloop is
    /// allowed to start before it has to re-synchronize to the blockchain
    /// by waiting for the next block to appear.
    pub max_run_loop_delay: Duration,
    pub max_winners_per_auction: usize,
    pub max_solutions_per_solver: usize,
}

pub struct RunLoop {
    config: Config,
    eth: infra::Ethereum,
    persistence: infra::Persistence,
    drivers: Vec<Arc<infra::Driver>>,
    solver_participation_guard: SolverParticipationGuard,
    solvable_orders_cache: Arc<SolvableOrdersCache>,
    trusted_tokens: AutoUpdatingTokenList,
    in_flight_orders: Arc<Mutex<HashSet<OrderUid>>>,
    liveness: Arc<Liveness>,
    /// Maintenance tasks that should run before every runloop to have
    /// the most recent data available.
    maintenance: Arc<Maintenance>,
    competition_updates_sender: tokio::sync::mpsc::UnboundedSender<()>,
}

impl RunLoop {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        eth: infra::Ethereum,
        persistence: infra::Persistence,
        drivers: Vec<Arc<infra::Driver>>,
        solver_participation_guard: SolverParticipationGuard,
        solvable_orders_cache: Arc<SolvableOrdersCache>,
        trusted_tokens: AutoUpdatingTokenList,
        liveness: Arc<Liveness>,
        maintenance: Arc<Maintenance>,
        competition_updates_sender: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Self {
        Self {
            config,
            eth,
            persistence,
            drivers,
            solver_participation_guard,
            solvable_orders_cache,
            trusted_tokens,
            in_flight_orders: Default::default(),
            liveness,
            maintenance,
            competition_updates_sender,
        }
    }

    pub async fn run_forever(self) -> ! {
        Maintenance::spawn_cow_amm_indexing_task(
            self.maintenance.clone(),
            self.eth.current_block().clone(),
        );
        let mut last_auction = None;
        let mut last_block = None;
        let self_arc = Arc::new(self);
        loop {
            let auction = self_arc
                .next_auction(&mut last_auction, &mut last_block)
                .await;
            if let Some(auction) = auction {
                let auction_id = auction.id;
                self_arc
                    .single_run(auction)
                    .instrument(tracing::info_span!("auction", auction_id))
                    .await
            };
        }
    }

    /// Sleeps until the next auction is supposed to start, builds it and
    /// returns it.
    async fn next_auction(
        &self,
        prev_auction: &mut Option<domain::Auction>,
        prev_block: &mut Option<H256>,
    ) -> Option<domain::Auction> {
        // wait for appropriate time to start building the auction
        let start_block = {
            let current_block = *self.eth.current_block().borrow();
            let time_since_last_block = current_block.observed_at.elapsed();
            let auction_block = if time_since_last_block > self.config.max_run_loop_delay {
                if prev_block.is_some_and(|prev_block| prev_block != current_block.hash) {
                    // don't emit warning if we finished prev run loop within the same block
                    tracing::warn!(
                        missed_by = ?time_since_last_block - self.config.max_run_loop_delay,
                        "missed optimal auction start, wait for new block"
                    );
                }
                ethrpc::block_stream::next_block(self.eth.current_block()).await
            } else {
                current_block
            };

            self.run_maintenance(&auction_block).await;
            match self
                .solvable_orders_cache
                .update(auction_block.number)
                .await
            {
                Ok(()) => {
                    self.solvable_orders_cache.track_auction_update("success");
                }
                Err(err) => {
                    self.solvable_orders_cache.track_auction_update("failure");
                    tracing::warn!(?err, "failed to update auction");
                }
            }
            auction_block
        };

        let auction = self.cut_auction().await?;

        // Only run the solvers if the auction or block has changed.
        let previous = prev_auction.replace(auction.clone());
        if previous.as_ref() == Some(&auction)
            && prev_block.replace(start_block.hash) == Some(start_block.hash)
        {
            return None;
        }

        observe::log_auction_delta(&previous, &auction);
        self.liveness.auction();
        Metrics::auction_ready(start_block.observed_at);
        Some(auction)
    }

    /// Runs maintenance on all components to ensure the system uses
    /// the latest available state.
    async fn run_maintenance(&self, block: &BlockInfo) {
        let start = Instant::now();
        self.maintenance.update(block).await;
        Metrics::ran_maintenance(start.elapsed());
    }

    async fn cut_auction(&self) -> Option<domain::Auction> {
        let auction = match self.solvable_orders_cache.current_auction().await {
            Some(auction) => auction,
            None => {
                tracing::debug!("no current auction");
                return None;
            }
        };
        let auction = self.remove_in_flight_orders(auction).await;

        let id = match self.persistence.replace_current_auction(&auction).await {
            Ok(id) => {
                Metrics::auction(id);
                id
            }
            Err(err) => {
                tracing::error!(?err, "failed to replace current auction");
                return None;
            }
        };

        if auction.orders.is_empty() {
            // Updating liveness probe to not report unhealthy due to this optimization
            self.liveness.auction();
            tracing::debug!("skipping empty auction");
            return None;
        }

        Some(domain::Auction {
            id,
            block: auction.block,
            orders: auction.orders,
            prices: auction.prices,
            surplus_capturing_jit_order_owners: auction.surplus_capturing_jit_order_owners,
        })
    }

    async fn single_run(self: &Arc<Self>, auction: domain::Auction) {
        let single_run_start = Instant::now();
        tracing::info!(auction_id = ?auction.id, "solving");

        // Mark all auction orders as `Ready` for competition
        self.persistence
            .store_order_events(auction.orders.iter().map(|o| o.uid), OrderEventLabel::Ready);

        // Collect valid solutions from all drivers
        let solutions = self.competition(&auction).await;
        observe::solutions(&solutions);
        if solutions.is_empty() {
            return;
        }

        let competition_simulation_block = self.eth.current_block().borrow().number;
        let block_deadline = competition_simulation_block + self.config.submission_deadline;

        // Post-processing should not be executed asynchronously since it includes steps
        // of storing all the competition/auction-related data to the DB.
        if let Err(err) = self
            .post_processing(
                &auction,
                competition_simulation_block,
                &solutions,
                block_deadline,
            )
            .await
        {
            tracing::error!(?err, "failed to post-process competition");
            return;
        }

        // Mark all winning orders as `Executing`
        let winning_orders = solutions
            .iter()
            .filter(|p| p.is_winner())
            .flat_map(|p| p.solution().order_ids().copied())
            .collect::<HashSet<_>>();
        self.persistence
            .store_order_events(winning_orders.clone(), OrderEventLabel::Executing);

        // Mark the rest as `Considered` for execution
        self.persistence.store_order_events(
            solutions
                .iter()
                .flat_map(|p| p.solution().order_ids().copied())
                .filter(|order_id| !winning_orders.contains(order_id)),
            OrderEventLabel::Considered,
        );

        for winner in solutions
            .iter()
            .filter(|participant| participant.is_winner())
        {
            let (driver, solution) = (winner.driver(), winner.solution());
            tracing::info!(driver = %driver.name, solution = %solution.id(), "winner");

            self.start_settlement_execution(
                auction.id,
                single_run_start,
                driver,
                solution,
                block_deadline,
            )
            .await;
        }
        observe::unsettled(&solutions, &auction);
    }

    /// Starts settlement execution in a background task. The function is async
    /// only to get access to the locks.
    async fn start_settlement_execution(
        self: &Arc<Self>,
        auction_id: Id,
        single_run_start: Instant,
        driver: &Arc<infra::Driver>,
        solution: &Solution,
        block_deadline: u64,
    ) {
        let solved_order_uids: HashSet<_> = solution.orders().keys().cloned().collect();
        self.in_flight_orders
            .lock()
            .await
            .extend(solved_order_uids.clone());

        let solution_id = solution.id();
        let solver = solution.solver();
        let self_ = self.clone();
        let driver_ = driver.clone();

        let settle_fut = async move {
            tracing::info!(driver = %driver_.name, solution = %solution_id, "settling");
            let submission_start = Instant::now();

            match self_
                .settle(
                    &driver_,
                    solution_id,
                    solved_order_uids.clone(),
                    solver,
                    auction_id,
                    block_deadline,
                )
                .await
            {
                Ok(tx_hash) => {
                    Metrics::settle_ok(
                        &driver_,
                        solved_order_uids.len(),
                        submission_start.elapsed(),
                    );
                    tracing::debug!(?tx_hash, driver = %driver_.name, ?solver, "solution settled");
                }
                Err(err) => {
                    Metrics::settle_err(&driver_, submission_start.elapsed(), &err);
                    tracing::warn!(?err, driver = %driver_.name, "settlement failed");
                }
            }
            Metrics::single_run_completed(single_run_start.elapsed());
        }
        .instrument(tracing::Span::current());

        tokio::spawn(settle_fut);
    }

    async fn post_processing(
        &self,
        auction: &domain::Auction,
        competition_simulation_block: u64,
        solutions: &[competition::Participant],
        block_deadline: u64,
    ) -> Result<()> {
        let start = Instant::now();
        // TODO: Needs to be removed once other teams fully migrated to the
        // reference_scores table
        let Some(winning_solution) = solutions
            .iter()
            .find(|participant| participant.is_winner())
            .map(|participant| participant.solution())
        else {
            return Err(anyhow::anyhow!("no winners found"));
        };
        let winner = winning_solution.solver().into();
        let winning_score = winning_solution.score().get().0;
        let reference_score = solutions
            .get(1)
            .map(|participant| participant.solution().score().get().0)
            .unwrap_or_default();

        let participants = solutions
            .iter()
            .map(|participant| participant.solution().solver().into())
            .collect::<HashSet<_>>();
        let mut fee_policies = Vec::new();
        for order_id in solutions
            .iter()
            .flat_map(|participant| participant.solution().order_ids())
            .unique()
        {
            match auction
                .orders
                .iter()
                .find(|auction_order| &auction_order.uid == order_id)
            {
                Some(auction_order) => {
                    fee_policies.push((auction_order.uid, auction_order.protocol_fees.clone()));
                }
                None => {
                    tracing::debug!(?order_id, "order not found in auction");
                }
            }
        }

        let competition_table = SolverCompetitionDB {
            auction_start_block: auction.block,
            competition_simulation_block,
            auction: CompetitionAuction {
                orders: auction
                    .orders
                    .iter()
                    .map(|order| order.uid.into())
                    .collect(),
                prices: auction
                    .prices
                    .iter()
                    .map(|(key, value)| ((*key).into(), value.get().into()))
                    .collect(),
            },
            solutions: solutions
                .iter()
                // reverse as solver competition table is sorted from worst to best, so we need to keep the ordering for backwards compatibility
                .rev()
                .enumerate()
                .map(|(index, participant)| SolverSettlement {
                    solver: participant.driver().name.clone(),
                    solver_address: participant.solution().solver().0,
                    score: Some(Score::Solver(participant.solution().score().get().0)),
                    ranking: solutions.len() - index,
                    orders: participant
                        .solution()
                        .orders()
                        .iter()
                        .map(|(id, order)| Order::Colocated {
                            id: (*id).into(),
                            sell_amount: order.executed_sell.into(),
                            buy_amount: order.executed_buy.into(),
                        })
                        .collect(),
                    clearing_prices: participant
                        .solution()
                        .prices()
                        .iter()
                        .map(|(token, price)| (token.0, price.get().into()))
                        .collect(),
                    is_winner: participant.is_winner(),
                })
                .collect(),
        };
        let competition = Competition {
            auction_id: auction.id,
            winner,
            winning_score,
            reference_score,
            participants,
            prices: auction
                .prices
                .clone()
                .into_iter()
                .map(|(key, value)| (key.into(), value.get().into()))
                .collect(),
            block_deadline,
            competition_simulation_block,
            competition_table,
        };

        match futures::try_join!(
            self.persistence
                .save_auction(auction, block_deadline)
                .map_err(|e| e.0.context("failed to save auction")),
            self.persistence
                .save_solutions(auction.id, solutions)
                .map_err(|e| e.0.context("failed to save solutions")),
        ) {
            Ok(_) => {
                // Notify the solver participation guard that the proposed solutions have been
                // saved.
                if let Err(err) = self.competition_updates_sender.send(()) {
                    tracing::error!(?err, "failed to notify solver participation guard");
                }
            }
            Err(err) => {
                // Don't error if saving of auction and solution fails, until stable.
                // Various edge cases with JIT orders verifiable only in production.
                tracing::warn!(?err, "failed to save new competition data");
            }
        }

        tracing::trace!(?competition, "saving competition");
        futures::try_join!(
            self.persistence
                .save_competition(&competition)
                .map_err(|e| e.0.context("failed to save competition")),
            self.persistence
                .save_surplus_capturing_jit_order_owners(
                    auction.id,
                    &auction.surplus_capturing_jit_order_owners,
                )
                .map_err(|e| e.0.context("failed to save jit order owners")),
            self.persistence
                .store_fee_policies(auction.id, fee_policies)
                .map_err(|e| e.context("failed to fee_policies")),
        )?;

        Metrics::post_processed(start.elapsed());
        Ok(())
    }

    /// Runs the solver competition, making all configured drivers participate.
    /// Returns all fair solutions sorted by their score (best to worst).
    async fn competition(&self, auction: &domain::Auction) -> Vec<competition::Participant> {
        let request = solve::Request::new(
            auction,
            &self.trusted_tokens.all(),
            self.config.solve_deadline,
        );
        let request = &request;

        let mut solutions = futures::future::join_all(
            self.drivers
                .iter()
                .map(|driver| self.solve(driver.clone(), request)),
        )
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

        // Shuffle so that sorting randomly splits ties.
        solutions.shuffle(&mut rand::thread_rng());
        solutions.sort_unstable_by_key(|participant| {
            std::cmp::Reverse(participant.solution().score().get().0)
        });

        // Filter out solutions that don't come from their corresponding submission
        // address
        let mut solutions = solutions
            .into_iter()
            .filter(|participant| {
                let submission_address = participant.driver().submission_address;
                let is_solution_from_driver = participant.solution().solver() == submission_address;
                if !is_solution_from_driver {
                    tracing::warn!(
                        driver = participant.driver().name,
                        ?submission_address,
                        "the solution received is not from the driver submission address"
                    );
                }
                is_solution_from_driver
            })
            .collect::<Vec<_>>();

        // Limit the number of accepted solutions per solver. Do not alter the ordering
        // of solutions
        let mut counter = HashMap::new();
        solutions.retain(|participant| {
            let driver = participant.driver().name.clone();
            let count = counter.entry(driver).or_insert(0);
            *count += 1;
            *count <= self.config.max_solutions_per_solver
        });

        // Filter out solutions that are not fair
        let solutions = solutions
            .iter()
            .enumerate()
            .filter_map(|(index, participant)| {
                if Self::is_solution_fair(participant, &solutions[index..], auction) {
                    Some(participant)
                } else {
                    tracing::warn!(
                        invalidated = participant.driver().name,
                        "fairness check invalidated of solution"
                    );
                    None
                }
            });

        // Winners are selected one by one, starting from the best solution,
        // until `max_winners_per_auction` are selected. The solution is a winner
        // if it swaps tokens that are not yet swapped by any previously processed
        // solution.
        let wrapped_native_token = self.eth.contracts().wrapped_native_token();
        let mut already_swapped_tokens = HashSet::new();
        let mut winners = 0;
        let solutions = solutions
            .cloned()
            .map(|participant| {
                let swapped_tokens = participant
                    .solution()
                    .orders()
                    .iter()
                    .flat_map(|(_, order)| {
                        [
                            order.sell.token.as_erc20(wrapped_native_token),
                            order.buy.token.as_erc20(wrapped_native_token),
                        ]
                    })
                    .collect::<HashSet<_>>();

                let is_winner = swapped_tokens.is_disjoint(&already_swapped_tokens)
                    && winners < self.config.max_winners_per_auction;

                already_swapped_tokens.extend(swapped_tokens);
                winners += usize::from(is_winner);

                participant.rank(is_winner)
            })
            .collect();

        solutions
    }

    /// Returns true if solution is fair to other solutions
    fn is_solution_fair(
        solution: &competition::Participant<Unranked>,
        others: &[competition::Participant<Unranked>],
        auction: &domain::Auction,
    ) -> bool {
        let Some(fairness_threshold) = solution.driver().fairness_threshold else {
            return true;
        };

        // Returns the surplus difference in the buy token if `left`
        // is better for the trader than `right`, or 0 otherwise.
        // This takes differently partial fills into account.
        let improvement_in_buy = |left: &TradedOrder, right: &TradedOrder| {
            // If `left.sell / left.buy < right.sell / right.buy`, left is "better" as the
            // trader either sells less or gets more. This can be reformulated as
            // `right.sell * left.buy > left.sell * right.buy`.
            let right_sell_left_buy = right.executed_sell.0.full_mul(left.executed_buy.0);
            let left_sell_right_buy = left.executed_sell.0.full_mul(right.executed_buy.0);
            let improvement = right_sell_left_buy
                .checked_sub(left_sell_right_buy)
                .unwrap_or_default();

            // The difference divided by the original sell amount is the improvement in buy
            // token. Casting to U256 is safe because the difference is smaller than the
            // original product, which if re-divided by right.sell must fit in U256.
            improvement
                .checked_div(right.executed_sell.0.into())
                .map(|v| U256::try_from(v).expect("improvement in buy fits in U256"))
                .unwrap_or_default()
        };

        // Record best execution per order
        let mut best_executions = HashMap::new();
        for other in others {
            for (uid, execution) in other.solution().orders() {
                best_executions
                    .entry(uid)
                    .and_modify(|best_execution| {
                        if !improvement_in_buy(execution, best_execution).is_zero() {
                            *best_execution = *execution;
                        }
                    })
                    .or_insert(*execution);
            }
        }

        // Check if the solution contains an order whose execution in the
        // solution is more than `fairness_threshold` worse than the
        // order's best execution across all solutions
        let unfair = solution
            .solution()
            .orders()
            .iter()
            .any(|(uid, current_execution)| {
                let best_execution = best_executions.get(uid).expect("by construction above");
                let improvement = improvement_in_buy(best_execution, current_execution);
                if improvement.is_zero() {
                    return false;
                };
                tracing::debug!(
                    ?uid,
                    ?improvement,
                    ?best_execution,
                    ?current_execution,
                    "fairness check"
                );
                // Improvement is denominated in buy token, use buy price to normalize the
                // difference into eth
                let Some(order) = auction.orders.iter().find(|order| order.uid == *uid) else {
                    // This can happen for jit orders
                    tracing::debug!(?uid, "cannot ensure fairness, order not found in auction");
                    return false;
                };
                let Some(buy_price) = auction.prices.get(&order.buy.token) else {
                    tracing::warn!(
                        ?order,
                        "cannot ensure fairness, buy price not found in auction"
                    );
                    return false;
                };
                buy_price.in_eth(improvement.into()) > fairness_threshold
            });
        !unfair
    }

    /// Sends a `/solve` request to the driver and manages all error cases and
    /// records metrics and logs appropriately.
    async fn solve(
        &self,
        driver: Arc<infra::Driver>,
        request: &solve::Request,
    ) -> Vec<competition::Participant<Unranked>> {
        let start = Instant::now();
        let result = self.try_solve(&driver, request).await;
        let solutions = match result {
            Ok(solutions) => {
                Metrics::solve_ok(&driver, start.elapsed());
                solutions
            }
            Err(err) => {
                Metrics::solve_err(&driver, start.elapsed(), &err);
                if matches!(err, SolveError::NoSolutions) {
                    tracing::debug!(driver = %driver.name, "solver found no solution");
                } else {
                    tracing::warn!(?err, driver = %driver.name, "solve error");
                }
                vec![]
            }
        };

        solutions
            .into_iter()
            .filter_map(|solution| match solution {
                Ok(solution) => {
                    Metrics::solution_ok(&driver);
                    Some(competition::Participant::new(solution, driver.clone()))
                }
                Err(err) => {
                    Metrics::solution_err(&driver, &err);
                    tracing::debug!(?err, driver = %driver.name, "invalid proposed solution");
                    None
                }
            })
            .collect()
    }

    /// Sends `/solve` request to the driver and forwards errors to the caller.
    async fn try_solve(
        &self,
        driver: &infra::Driver,
        request: &solve::Request,
    ) -> Result<Vec<Result<competition::Solution, domain::competition::SolutionError>>, SolveError>
    {
        let can_participate = self.solver_participation_guard.can_participate(&driver.submission_address).await.map_err(|err| {
            tracing::error!(?err, driver = %driver.name, ?driver.submission_address, "solver participation check failed");
                SolveError::SolverDenyListed
            }
        )?;

        // Do not send the request to the driver if the solver is deny-listed
        if !can_participate {
            return Err(SolveError::SolverDenyListed);
        }

        let response = tokio::time::timeout(self.config.solve_deadline, driver.solve(request))
            .await
            .map_err(|_| SolveError::Timeout)?
            .map_err(SolveError::Failure)?;
        if response.solutions.is_empty() {
            return Err(SolveError::NoSolutions);
        }
        Ok(response.into_domain())
    }

    /// Execute the solver's solution. Returns Ok when the corresponding
    /// transaction has been mined.
    async fn settle(
        &self,
        driver: &infra::Driver,
        solution_id: u64,
        solved_order_uids: HashSet<OrderUid>,
        solver: eth::Address,
        auction_id: i64,
        submission_deadline_latest_block: u64,
    ) -> Result<TxId, SettleError> {
        let settle = async move {
            let current_block = self.eth.current_block().borrow().number;
            anyhow::ensure!(
                current_block < submission_deadline_latest_block,
                "submission deadline was missed"
            );

            let request = settle::Request {
                solution_id,
                submission_deadline_latest_block,
                auction_id,
            };

            self.store_execution_started(
                auction_id,
                solver,
                current_block,
                submission_deadline_latest_block,
            );
            driver
                .settle(&request, self.config.max_settlement_transaction_wait)
                .await
        }
        .boxed();

        let wait_for_settlement_transaction = self
            .wait_for_settlement_transaction(auction_id, solver, submission_deadline_latest_block)
            .boxed();

        // Wait for either the settlement transaction to be mined or the driver returned
        // a result.
        let result = match futures::future::select(wait_for_settlement_transaction, settle).await {
            futures::future::Either::Left((res, _)) => res,
            futures::future::Either::Right((driver_result, wait_for_settlement_transaction)) => {
                match driver_result {
                    Ok(_) => wait_for_settlement_transaction.await,
                    Err(err) => Err(SettleError::Other(err)),
                }
            }
        };

        self.store_execution_ended(solver, auction_id, &result);

        // Clean up the in-flight orders regardless the result.
        self.in_flight_orders
            .lock()
            .await
            .retain(|order| !solved_order_uids.contains(order));

        result
    }

    /// Stores settlement execution started event in the DB in a background task
    /// to not block the runloop.
    fn store_execution_started(
        &self,
        auction_id: i64,
        solver: eth::Address,
        start_block: u64,
        deadline_block: u64,
    ) {
        let persistence = self.persistence.clone();
        tokio::spawn(async move {
            let execution_started = ExecutionStarted {
                auction_id,
                solver,
                start_timestamp: chrono::Utc::now(),
                start_block,
                deadline_block,
            };

            if let Err(err) = persistence
                .store_settlement_execution_started(execution_started)
                .await
            {
                tracing::error!(?err, "failed to store settlement execution event");
            }
        });
    }

    /// Stores settlement execution ended event in the DB in a background task
    /// to not block the runloop.
    fn store_execution_ended(
        &self,
        solver: eth::Address,
        auction_id: i64,
        result: &Result<TxId, SettleError>,
    ) {
        let end_timestamp = chrono::Utc::now();
        let current_block = self.eth.current_block().borrow().number;
        let persistence = self.persistence.clone();
        let outcome = match result {
            Ok(_) => "success".to_string(),
            Err(SettleError::Timeout) => "timeout".to_string(),
            Err(SettleError::Other(err)) => format!("driver failed: {}", err),
        };

        tokio::spawn(async move {
            let execution_ended = ExecutionEnded {
                auction_id,
                solver,
                end_timestamp,
                end_block: current_block,
                outcome,
            };
            if let Err(err) = persistence
                .store_settlement_execution_ended(execution_ended)
                .await
            {
                tracing::error!(?err, "failed to update settlement execution event");
            }
        });
    }

    /// Tries to find a `settle` contract call with calldata ending in `tag` and
    /// originated from the `solver`.
    ///
    /// Returns None if no transaction was found within the deadline or the task
    /// is cancelled.
    async fn wait_for_settlement_transaction(
        &self,
        auction_id: i64,
        solver: eth::Address,
        submission_deadline_latest_block: u64,
    ) -> Result<eth::TxId, SettleError> {
        let current = self.eth.current_block().borrow().number;
        tracing::debug!(%current, deadline=%submission_deadline_latest_block, %auction_id, "waiting for tag");
        loop {
            let block = ethrpc::block_stream::next_block(self.eth.current_block()).await;
            // Run maintenance to ensure the system processed the last available block so
            // it's possible to find the tx in the DB in the next line.
            self.run_maintenance(&block).await;

            match self
                .persistence
                .find_settlement_transaction(auction_id, solver)
                .await
            {
                Ok(Some(transaction)) => return Ok(transaction),
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        ?auction_id,
                        ?solver,
                        "failed to find settlement transaction"
                    );
                }
            }
            if block.number >= submission_deadline_latest_block {
                break;
            }
        }
        Err(SettleError::Timeout)
    }

    /// Removes orders that are currently being settled to avoid solvers trying
    /// to fill an order a second time.
    async fn remove_in_flight_orders(
        &self,
        mut auction: domain::RawAuctionData,
    ) -> domain::RawAuctionData {
        let in_flight = &*self.in_flight_orders.lock().await;
        if in_flight.is_empty() {
            return auction;
        };

        auction.orders.retain(|o| !in_flight.contains(&o.uid));
        auction
            .surplus_capturing_jit_order_owners
            .retain(|owner| !in_flight.iter().any(|i| i.owner() == *owner));
        tracing::debug!(
            orders = ?in_flight,
            "filtered out in-flight orders and surplus_capturing_jit_order_owners"
        );

        auction
    }
}

#[derive(Debug, thiserror::Error)]
enum SolveError {
    #[error("the solver timed out")]
    Timeout,
    #[error("driver did not propose any solutions")]
    NoSolutions,
    #[error(transparent)]
    Failure(anyhow::Error),
    #[error("the solver got deny listed")]
    SolverDenyListed,
}

#[derive(Debug, thiserror::Error)]
enum SettleError {
    #[error(transparent)]
    Other(anyhow::Error),
    #[error("settlement transaction await reached deadline")]
    Timeout,
}

#[derive(prometheus_metric_storage::MetricStorage)]
#[metric(subsystem = "runloop")]
struct Metrics {
    /// Tracks the last executed auction.
    auction: prometheus::IntGauge,

    /// Tracks the duration of successful driver `/solve` requests.
    #[metric(
        labels("driver", "result"),
        buckets(
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20
        )
    )]
    solve: prometheus::HistogramVec,

    /// Tracks driver solutions.
    #[metric(labels("driver", "result"))]
    solutions: prometheus::IntCounterVec,

    /// Tracks the result of driver `/reveal` requests.
    #[metric(labels("driver", "result"))]
    reveal: prometheus::HistogramVec,

    /// Tracks the times and results of driver `/settle` requests.
    #[metric(
        labels("driver", "result"),
        buckets(0, 3, 6, 9, 12, 15, 18, 21, 24, 27, 30, 33, 36, 39, 42, 45, 48)
    )]
    settle: prometheus::HistogramVec,

    /// Tracks the number of orders that were part of some but not the winning
    /// solution together with the winning driver that did't include it.
    #[metric(labels("ignored_by"))]
    matched_unsettled: prometheus::IntCounterVec,

    /// Tracks the number of orders that were settled together with the
    /// settling driver.
    #[metric(labels("driver"))]
    settled: prometheus::IntCounterVec,

    /// Tracks the number of database errors.
    #[metric(labels("error_type"))]
    db_metric_error: prometheus::IntCounterVec,

    /// Tracks the time spent in post-processing after the auction has been
    /// solved and before sending a `settle` request.
    auction_postprocessing_time: prometheus::Histogram,

    /// Tracks the time spent running maintenance. This mostly consists of
    /// indexing new events.
    #[metric(buckets(0.01, 0.05, 0.1, 0.2, 0.5, 1, 1.5, 2, 2.5, 5))]
    service_maintenance_time: prometheus::Histogram,

    /// Total time spent in a single run of the run loop.
    #[metric(buckets(0, 3, 6, 9, 12, 15, 18, 21, 24, 27, 30, 33, 36, 39, 42, 45, 48))]
    single_run_time: prometheus::Histogram,

    /// Time difference between the current block and when the single run
    /// function is started.
    #[metric(buckets(0, 0.25, 0.5, 0.75, 1, 1.5, 2, 2.5, 3, 4, 5, 6))]
    current_block_delay: prometheus::Histogram,
}

impl Metrics {
    fn get() -> &'static Self {
        Metrics::instance(metrics::get_storage_registry()).unwrap()
    }

    fn auction(auction_id: domain::auction::Id) {
        Self::get().auction.set(auction_id)
    }

    fn solve_ok(driver: &infra::Driver, elapsed: Duration) {
        Self::get()
            .solve
            .with_label_values(&[&driver.name, "success"])
            .observe(elapsed.as_secs_f64())
    }

    fn solve_err(driver: &infra::Driver, elapsed: Duration, err: &SolveError) {
        let label = match err {
            SolveError::Timeout => "timeout",
            SolveError::NoSolutions => "no_solutions",
            SolveError::Failure(_) => "error",
            SolveError::SolverDenyListed => "deny_listed",
        };
        Self::get()
            .solve
            .with_label_values(&[&driver.name, label])
            .observe(elapsed.as_secs_f64())
    }

    fn solution_ok(driver: &infra::Driver) {
        Self::get()
            .solutions
            .with_label_values(&[&driver.name, "success"])
            .inc();
    }

    fn solution_err(driver: &infra::Driver, err: &SolutionError) {
        let label = match err {
            SolutionError::ZeroScore(_) => "zero_score",
            SolutionError::InvalidPrice(_) => "invalid_price",
            SolutionError::SolverDenyListed => "solver_deny_listed",
        };
        Self::get()
            .solutions
            .with_label_values(&[&driver.name, label])
            .inc();
    }

    fn settle_ok(driver: &infra::Driver, settled_order_count: usize, elapsed: Duration) {
        Self::get()
            .settle
            .with_label_values(&[&driver.name, "success"])
            .observe(elapsed.as_secs_f64());
        Self::get()
            .settled
            .with_label_values(&[&driver.name])
            .inc_by(settled_order_count.try_into().unwrap_or(u64::MAX));
    }

    fn settle_err(driver: &infra::Driver, elapsed: Duration, err: &SettleError) {
        let label = match err {
            SettleError::Other(_) => "error",
            SettleError::Timeout => "timeout",
        };
        Self::get()
            .settle
            .with_label_values(&[&driver.name, label])
            .observe(elapsed.as_secs_f64());
    }

    fn matched_unsettled(winning: &infra::Driver, unsettled: HashSet<&domain::OrderUid>) {
        if !unsettled.is_empty() {
            tracing::debug!(?unsettled, "some orders were matched but not settled");
        }
        Self::get()
            .matched_unsettled
            .with_label_values(&[&winning.name])
            .inc_by(unsettled.len() as u64);
    }

    fn post_processed(elapsed: Duration) {
        Self::get()
            .auction_postprocessing_time
            .observe(elapsed.as_secs_f64());
    }

    fn ran_maintenance(elapsed: Duration) {
        Self::get()
            .service_maintenance_time
            .observe(elapsed.as_secs_f64());
    }

    fn single_run_completed(elapsed: Duration) {
        Self::get().single_run_time.observe(elapsed.as_secs_f64());
    }

    fn auction_ready(init_block_timestamp: Instant) {
        Self::get()
            .current_block_delay
            .observe(init_block_timestamp.elapsed().as_secs_f64())
    }
}

pub mod observe {
    use {crate::domain, std::collections::HashSet};

    pub fn log_auction_delta(previous: &Option<domain::Auction>, current: &domain::Auction) {
        let previous_uids = match previous {
            Some(previous) => previous
                .orders
                .iter()
                .map(|order| order.uid)
                .collect::<HashSet<_>>(),
            None => HashSet::new(),
        };
        let current_uids = current
            .orders
            .iter()
            .map(|order| order.uid)
            .collect::<HashSet<_>>();
        let added = current_uids.difference(&previous_uids);
        let removed = previous_uids.difference(&current_uids);
        tracing::debug!(
            id = current.id,
            added = ?added,
            "New orders in auction"
        );
        tracing::debug!(
            id = current.id,
            removed = ?removed,
            "Orders no longer in auction"
        );
    }

    pub fn solutions(solutions: &[domain::competition::Participant]) {
        if solutions.is_empty() {
            tracing::info!("no solutions for auction");
        }
        for participant in solutions {
            tracing::debug!(
                driver = %participant.driver().name,
                orders = ?participant.solution().order_ids(),
                solution = %participant.solution().id(),
                "proposed solution"
            );
        }
    }

    /// Records metrics for the matched but unsettled orders.
    pub fn unsettled(solutions: &[domain::competition::Participant], auction: &domain::Auction) {
        let Some(winner) = solutions.first() else {
            // no solutions means nothing to report
            return;
        };

        let mut non_winning_orders = {
            let winning_orders = solutions
                .iter()
                .filter(|p| p.is_winner())
                .flat_map(|p| p.solution().order_ids())
                .collect::<HashSet<_>>();
            solutions
                .iter()
                .flat_map(|p| p.solution().order_ids())
                .filter(|uid| !winning_orders.contains(uid))
                .collect::<HashSet<_>>()
        };
        // Report orders that were part of a non-winning solution candidate
        // but only if they were part of the auction (filter out jit orders)
        let auction_uids = auction.orders.iter().map(|o| o.uid).collect::<HashSet<_>>();
        non_winning_orders.retain(|uid| auction_uids.contains(uid));
        super::Metrics::matched_unsettled(winner.driver(), non_winning_orders);
    }
}
