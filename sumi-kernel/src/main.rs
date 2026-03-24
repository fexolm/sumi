#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]

#[cfg(not(test))]
mod kernel_main;
#[cfg(test)]
mod test_main;
