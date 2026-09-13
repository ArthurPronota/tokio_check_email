use std::{collections::HashSet, sync::Arc};

use tokio::{self, sync::RwLock, task::spawn_blocking} ;
use serde_json::{self, Value} ;
use futures::{stream, StreamExt} ;
use log::error ;
use emval ;

async fn check_emails(
            json_in: &str,
            valid_hosts: Arc<RwLock<HashSet<String>>>,
            invalid_hosts: Arc<RwLock<HashSet<String>>>,
        ) ->Result<Vec<String>, anyhow::Error> {

    let mut vec_out = vec![] ;

    let array = match serde_json::from_str::<Value>(json_in)? {
        Value::Array(a) => a,
        oth => {
            let oth_str = match oth {
                Value::Array(_) => "Array",
                Value::Bool(_) => "Bool",
                Value::Null => "Null",
                Value::Number(_) => "Number",
                Value::Object(_) => "Object",
                Value::String(_) => "String",
            };

            error!("json_in isn't Array, it's: {oth_str}") ;
            return Err(anyhow::anyhow!("json_in isn't Array, it's: {oth_str}"));
        },
    };

    let em_light_validator = emval::EmailValidator {
            deliverable_address: false,
            ..Default::default()
    } ;

    let mut stream_iter = stream::iter(array.iter()) ;

    while let Some(value) = stream_iter.next().await {
        if ! value.get("active").and_then(|a| a.as_bool()).unwrap_or(false) {
            continue;
        }

        let em_opt = value.get("email").and_then(|e| e.as_str()) ;

        match em_opt {
            None => continue,
            Some(em_str) => {
                match em_light_validator.validate_email(em_str) {
                    Err(err) => {
                        error!("{em_str}: {err}") ;
                        continue ;
                    },
                    Ok(e_v_l) => {
                        if invalid_hosts.read().await.contains(&e_v_l.domain_name) {
                            continue;
                        }

                        if ! valid_hosts.read().await.contains(&e_v_l.domain_name) {
                            let em_w = em_str.to_string() ;
                            match spawn_blocking(move || emval::validate_email(&em_w)).await {
                                Ok(v_all) => {
                                    match v_all {
                                        Ok(_) => {
                                            valid_hosts.write().await.insert(e_v_l.domain_name) ;
                                        },
                                        Err(err) => {
                                            invalid_hosts.write().await.insert(e_v_l.domain_name) ;
                                            error!("{em_str}: {err}") ;
                                            continue;
                                        },
                                    }
                                },
                                Err(err) => {
                                    error!("{err}") ;
                                    continue ;
                                }
                            }
                        }

                        vec_out.push(e_v_l.normalized);
                    }
                }
            },
        }

    }

    vec_out.sort();
    vec_out.dedup();

    Ok(vec_out)
}

#[tokio::main]
async fn main() ->Result<(), anyhow::Error>{
    env_logger::init();

    let json_str = r#"
[
  { "id": 1, "email": "user@mail.com", "active": true },
  { "id": 2, "email": null, "active": true },
  { "id": 3, "email": "invalid", "active": false },
  { "id": "wrong", "email": "test@test.com", "active": true },
  { "random": "data" }
]
    "# ;

    let valid_hosts = Arc::new(RwLock::new(HashSet::new())) ;
    let invalid_hosts = Arc::new(RwLock::new(HashSet::new())) ;

    println!("Validate emails: {:?}", check_emails(json_str, valid_hosts.clone(), invalid_hosts.clone()).await?) ;

    Ok(())
}
