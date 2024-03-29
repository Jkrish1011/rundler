// This file is part of Rundler.
//
// Rundler is free software: you can redistribute it and/or modify it under the
// terms of the GNU Lesser General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later version.
//
// Rundler is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
// without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with Rundler.
// If not, see https://www.gnu.org/licenses/.

use ethers::types::U256;

mod simulator;
pub use simulator::Simulator;

mod tracer;
pub use tracer::{SimulateValidationTracer, SimulateValidationTracerImpl};

mod unsafe_sim;
pub use unsafe_sim::UnsafeSimulator;

/// Required buffer for verification gas limit when targeting the 0.6 entrypoint contract
pub(crate) const REQUIRED_VERIFICATION_GAS_LIMIT_BUFFER: U256 = U256([2000, 0, 0, 0]);
