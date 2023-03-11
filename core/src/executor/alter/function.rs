use {
    super::{validate_arg, validate_arg_names, AlterError},
    crate::{
        ast::{Expr, OperateFunctionArg},
        data::CustomFunction,
        result::Result,
        store::{GStore, GStoreMut},
    },
};

pub async fn insert_function<T: GStore + GStoreMut>(
    storage: &mut T,
    func_name: &str,
    args: &Option<Vec<OperateFunctionArg>>,
    or_replace: bool,
    return_: &Option<Expr>,
) -> Result<()> {
    if let Some(args) = args {
        validate_arg_names(args)?;
        args.iter().try_for_each(validate_arg)?;
    }

    if storage.fetch_function(func_name).await?.is_none() || or_replace {
        storage.delete_function(func_name).await?;
        storage
            .insert_function(CustomFunction {
                func_name: func_name.to_owned(),
                args: args.to_owned(),
                return_: return_.to_owned(),
            })
            .await?;
        Ok(())
    } else {
        Err(AlterError::FunctionAlreadyExists(func_name.to_owned()).into())
    }
}

pub async fn delete_function<T: GStore + GStoreMut>(
    storage: &mut T,
    func_names: &[String],
    if_exists: bool,
) -> Result<()> {
    for func_name in func_names {
        let result = storage.delete_function(func_name).await;
        if result.is_err() && !if_exists {
            result?
        };
    }
    Ok(())
}
