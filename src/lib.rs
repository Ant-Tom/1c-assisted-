//! MVP оркестратора ИИ-ассистента, отвечающего на вопросы по данным 1С.
//!
//! Модель сама ничего не считает: числа берутся только из результатов функций чтения 1С.

pub mod auth;
pub mod config;
pub mod error;
pub mod llm;
pub mod onec;
pub mod tools;
