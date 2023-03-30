mod error;
mod evaluated;
mod expr;
mod function;
mod stateless;

use {
    super::{context::RowContext, select::select},
    crate::{
        ast::{Aggregate, Expr, Function},
        data::{Interval, Literal, Row, Value},
        result::{Error, Result},
        store::GStore,
    },
    async_recursion::async_recursion,
    chrono::prelude::Utc,
    futures::{
        future::{ready, try_join_all},
        stream::{self, StreamExt, TryStreamExt},
    },
    im_rc::HashMap,
    std::{borrow::Cow, collections::HashMap as StdHashMap, rc::Rc},
};

pub use {error::EvaluateError, evaluated::Evaluated, stateless::evaluate_stateless};

#[async_recursion(?Send)]
pub async fn evaluate<'a, 'b: 'a, 'c: 'a, T: GStore>(
    storage: &'a T,
    context: Option<Rc<RowContext<'b>>>,
    aggregated: Option<Rc<HashMap<&'c Aggregate, Value>>>,
    expr: &'a Expr,
) -> Result<Evaluated<'a>> {
    let eval = |expr| {
        let context = context.as_ref().map(Rc::clone);
        let aggregated = aggregated.as_ref().map(Rc::clone);

        evaluate(storage, context, aggregated, expr)
    };

    match expr {
        Expr::Literal(ast_literal) => expr::literal(ast_literal),
        Expr::TypedString { data_type, value } => {
            expr::typed_string(data_type, Cow::Borrowed(value))
        }
        Expr::Identifier(ident) => {
            let context = context.ok_or(EvaluateError::UnreachableEmptyContext)?;

            match context.get_value(ident) {
                Some(value) => Ok(value.clone()),
                None => Err(EvaluateError::ValueNotFound(ident.to_owned()).into()),
            }
            .map(Evaluated::from)
        }
        Expr::Nested(expr) => eval(expr).await,
        Expr::CompoundIdentifier { alias, ident } => {
            let table_alias = &alias;
            let column = &ident;
            let context = context.ok_or(EvaluateError::UnreachableEmptyContext)?;

            match context.get_alias_value(table_alias, column) {
                Some(value) => Ok(value.clone()),
                None => Err(EvaluateError::ValueNotFound(column.to_string()).into()),
            }
            .map(Evaluated::from)
        }
        Expr::Subquery(query) => {
            let evaluations = select(storage, query, context.as_ref().map(Rc::clone))
                .await?
                .map(|row| {
                    let value = match row? {
                        Row::Vec { values, .. } => values,
                        Row::Map(_) => {
                            return Err(EvaluateError::SchemalessProjectionForSubQuery.into());
                        }
                    }
                    .into_iter()
                    .next();

                    Ok::<_, Error>(value)
                })
                .take(2)
                .try_collect::<Vec<_>>()
                .await?;

            if evaluations.len() > 1 {
                return Err(EvaluateError::MoreThanOneRowReturned.into());
            }

            let value = evaluations
                .into_iter()
                .next()
                .flatten()
                .unwrap_or(Value::Null);

            Ok(Evaluated::from(value))
        }
        Expr::BinaryOp { op, left, right } => {
            let left = eval(left).await?;
            let right = eval(right).await?;

            expr::binary_op(op, left, right)
        }
        Expr::UnaryOp { op, expr } => {
            let v = eval(expr).await?;

            expr::unary_op(op, v)
        }
        Expr::Aggregate(aggr) => match aggregated
            .as_ref()
            .and_then(|aggregated| aggregated.get(aggr.as_ref()))
        {
            Some(value) => Ok(Evaluated::from(value.clone())),
            None => Err(EvaluateError::UnreachableEmptyAggregateValue(*aggr.clone()).into()),
        },
        Expr::Function(func) => {
            let context = context.as_ref().map(Rc::clone);
            let aggregated = aggregated.as_ref().map(Rc::clone);

            evaluate_function(storage, context, aggregated, func).await
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let negated = *negated;
            let target = eval(expr).await?;

            stream::iter(list)
                .then(eval)
                .try_filter(|evaluated| ready(evaluated == &target))
                .try_next()
                .await
                .map(|v| v.is_some() ^ negated)
                .map(Value::Bool)
                .map(Evaluated::from)
        }
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            let target = eval(expr).await?;

            select(storage, subquery, context)
                .await?
                .map(|row| {
                    let value = match row? {
                        Row::Vec { values, .. } => values,
                        Row::Map(_) => {
                            return Err(EvaluateError::SchemalessProjectionForInSubQuery.into());
                        }
                    }
                    .into_iter()
                    .next()
                    .unwrap_or(Value::Null);

                    Ok(Evaluated::from(value))
                })
                .try_filter(|evaluated| ready(evaluated == &target))
                .try_next()
                .await
                .map(|v| v.is_some() ^ negated)
                .map(Value::Bool)
                .map(Evaluated::from)
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let target = eval(expr).await?;
            let low = eval(low).await?;
            let high = eval(high).await?;

            expr::between(target, *negated, low, high)
        }
        Expr::Like {
            expr,
            negated,
            pattern,
        } => {
            let target = eval(expr).await?;
            let pattern = eval(pattern).await?;
            let evaluated = target.like(pattern, true)?;

            Ok(match negated {
                true => Evaluated::from(Value::Bool(
                    evaluated == Evaluated::Literal(Literal::Boolean(false)),
                )),
                false => evaluated,
            })
        }
        Expr::ILike {
            expr,
            negated,
            pattern,
        } => {
            let target = eval(expr).await?;
            let pattern = eval(pattern).await?;
            let evaluated = target.like(pattern, false)?;

            Ok(match negated {
                true => Evaluated::from(Value::Bool(
                    evaluated == Evaluated::Literal(Literal::Boolean(false)),
                )),
                false => evaluated,
            })
        }
        Expr::Exists { subquery, negated } => select(storage, subquery, context)
            .await?
            .try_next()
            .await
            .map(|v| v.is_some() ^ negated)
            .map(Value::Bool)
            .map(Evaluated::from),
        Expr::IsNull(expr) => {
            let v = eval(expr).await?.is_null();

            Ok(Evaluated::from(Value::Bool(v)))
        }
        Expr::IsNotNull(expr) => {
            let v = eval(expr).await?.is_null();

            Ok(Evaluated::from(Value::Bool(!v)))
        }
        Expr::Case {
            operand,
            when_then,
            else_result,
        } => {
            let operand = match operand {
                Some(op) => eval(op).await?,
                None => Evaluated::from(Value::Bool(true)),
            };

            for (when, then) in when_then.iter() {
                let when = eval(when).await?;

                if when.eq(&operand) {
                    return eval(then).await;
                }
            }

            match else_result {
                Some(er) => eval(er).await,
                None => Ok(Evaluated::from(Value::Null)),
            }
        }
        Expr::ArrayIndex { obj, indexes } => {
            let obj = eval(obj).await?;
            let indexes = try_join_all(indexes.iter().map(eval)).await?;
            expr::array_index(obj, indexes)
        }
        Expr::Interval {
            expr,
            leading_field,
            last_field,
        } => {
            let value = eval(expr)
                .await
                .and_then(Value::try_from)
                .map(String::from)?;

            Interval::try_from_literal(&value, *leading_field, *last_field)
                .map(Value::Interval)
                .map(Evaluated::from)
        }
    }
}

async fn evaluate_function<'a, 'b: 'a, 'c: 'a, T: GStore>(
    storage: &'a T,
    context: Option<Rc<RowContext<'b>>>,
    aggregated: Option<Rc<HashMap<&'c Aggregate, Value>>>,
    func: &'b Function,
) -> Result<Evaluated<'a>> {
    use function as f;

    let eval = |expr| {
        let context = context.as_ref().map(Rc::clone);
        let aggregated = aggregated.as_ref().map(Rc::clone);

        evaluate(storage, context, aggregated, expr)
    };

    let eval_with_context = |expr: &'a Expr, context: Rc<RowContext<'b>>| {
        let context = Some(Rc::clone(&context));
        let aggregated = aggregated.as_ref().map(Rc::clone);

        evaluate(storage, context, aggregated, expr)
    };

    let name = func.to_string();

    match func {
        // --- text ---
        Function::Concat(exprs) => {
            let exprs = stream::iter(exprs).then(eval).try_collect().await?;
            f::concat(exprs)
        }
        Function::Custom { name, exprs } => {
            let custom_func = storage
                .fetch_function(name)
                .await?
                .ok_or_else(|| EvaluateError::UnsupportedFunction(name.to_string()))?;
            let args: Vec<Evaluated<'_>> = stream::iter(exprs).then(eval).try_collect().await?;
            let args: Vec<Value> = args
                .into_iter()
                .map(|v| Value::try_from(v).unwrap())
                .collect();

            let empty = vec![];

            let fargs = custom_func.args.as_ref().unwrap_or(&empty);

            let dargs: Vec<Value> = if let Some(fargs) = &custom_func.args {
                let dargs: Vec<&Expr> = fargs.iter().filter_map(|y| y.default.as_ref()).collect();
                let dargs: Vec<Evaluated<'_>> =
                    stream::iter(dargs).then(eval).try_collect().await?;
                dargs
                    .into_iter()
                    .map(|expr| Value::try_from(expr).unwrap())
                    .collect()
            } else {
                vec![]
            };

            let min = fargs.len() - dargs.len();
            let max = fargs.len();

            let value = if (min..=max).contains(&args.len()) {
                let mut hm = StdHashMap::new();
                let mut id = 0;

                fargs
                    .iter()
                    .enumerate()
                    .try_for_each(|(i, farg)| -> Result<()> {
                        let arg = args.get(i).unwrap_or(&Value::Null);
                        arg.validate_type(&farg.data_type)?;
                        arg.validate_null(farg.default.is_some())?;
                        let value = if arg.is_null() {
                            &dargs[{
                                let tmp = id;
                                id += 1;
                                tmp
                            }]
                        } else {
                            arg
                        };
                        hm.insert(farg.name.to_owned(), value.to_owned());
                        Ok(())
                    })?;

                let row = Row::Map(hm);
                let rowcontext = RowContext::new(name, Cow::Owned(row), None);
                let context = Rc::new(rowcontext);

                if let Some(v) = &custom_func.return_ {
                    eval_with_context(v, context).await
                } else {
                    Ok(Evaluated::from(Value::Null))
                }
            } else {
                Err((EvaluateError::FunctionArgsLengthNotWithinRange {
                    name: custom_func.func_name.to_owned(),
                    expected_minimum: min,
                    expected_maximum: max,
                    found: args.len(),
                })
                .into())
            };

            Ok(value?)
        }
        Function::ConcatWs { separator, exprs } => {
            let separator = eval(separator).await?;
            let exprs = stream::iter(exprs).then(eval).try_collect().await?;
            f::concat_ws(name, separator, exprs)
        }
        Function::IfNull { expr, then } => f::ifnull(eval(expr).await?, eval(then).await?),
        Function::Lower(expr) => f::lower(name, eval(expr).await?),
        Function::Upper(expr) => f::upper(name, eval(expr).await?),
        Function::Left { expr, size } | Function::Right { expr, size } => {
            let expr = eval(expr).await?;
            let size = eval(size).await?;

            f::left_or_right(name, expr, size)
        }
        Function::Lpad { expr, size, fill } | Function::Rpad { expr, size, fill } => {
            let expr = eval(expr).await?;
            let size = eval(size).await?;
            let fill = match fill {
                Some(v) => Some(eval(v).await?),
                None => None,
            };

            f::lpad_or_rpad(name, expr, size, fill)
        }
        Function::Trim {
            expr,
            filter_chars,
            trim_where_field,
        } => {
            let expr = eval(expr).await?;
            let filter_chars = match filter_chars {
                Some(v) => Some(eval(v).await?),
                None => None,
            };

            expr.trim(name, filter_chars, trim_where_field)
        }
        Function::Ltrim { expr, chars } => {
            let expr = eval(expr).await?;
            let chars = match chars {
                Some(v) => Some(eval(v).await?),
                None => None,
            };

            expr.ltrim(name, chars)
        }
        Function::Rtrim { expr, chars } => {
            let expr = eval(expr).await?;
            let chars = match chars {
                Some(v) => Some(eval(v).await?),
                None => None,
            };

            expr.rtrim(name, chars)
        }
        Function::Reverse(expr) => {
            let expr = eval(expr).await?;

            f::reverse(name, expr)
        }
        Function::Repeat { expr, num } => {
            let expr = eval(expr).await?;
            let num = eval(num).await?;

            f::repeat(name, expr, num)
        }
        Function::Substr { expr, start, count } => {
            let expr = eval(expr).await?;
            let start = eval(start).await?;
            let count = match count {
                Some(v) => Some(eval(v).await?),
                None => None,
            };
            expr.substr(name, start, count)
        }
        Function::Ascii(expr) => f::ascii(name, eval(expr).await?),
        Function::Chr(expr) => f::chr(name, eval(expr).await?),

        // --- float ---
        Function::Abs(expr) => f::abs(name, eval(expr).await?),
        Function::Sign(expr) => f::sign(name, eval(expr).await?),
        Function::Sqrt(expr) => f::sqrt(eval(expr).await?),
        Function::Power { expr, power } => {
            let expr = eval(expr).await?;
            let power = eval(power).await?;

            f::power(name, expr, power)
        }
        Function::Ceil(expr) => f::ceil(name, eval(expr).await?),
        Function::Rand(expr) => {
            let expr = match expr {
                Some(v) => Some(eval(v).await?),
                None => None,
            };
            f::rand(name, expr)
        }
        Function::Round(expr) => f::round(name, eval(expr).await?),
        Function::Floor(expr) => f::floor(name, eval(expr).await?),
        Function::Radians(expr) => f::radians(name, eval(expr).await?),
        Function::Degrees(expr) => f::degrees(name, eval(expr).await?),
        Function::Pi() => Ok(Evaluated::from(Value::F64(std::f64::consts::PI))),
        Function::Exp(expr) => f::exp(name, eval(expr).await?),
        Function::Log { antilog, base } => {
            let antilog = eval(antilog).await?;
            let base = eval(base).await?;

            f::log(name, antilog, base)
        }
        Function::Ln(expr) => f::ln(name, eval(expr).await?),
        Function::Log2(expr) => f::log2(name, eval(expr).await?),
        Function::Log10(expr) => f::log10(name, eval(expr).await?),
        Function::Sin(expr) => f::sin(name, eval(expr).await?),
        Function::Cos(expr) => f::cos(name, eval(expr).await?),
        Function::Tan(expr) => f::tan(name, eval(expr).await?),
        Function::Asin(expr) => f::asin(name, eval(expr).await?),
        Function::Acos(expr) => f::acos(name, eval(expr).await?),
        Function::Atan(expr) => f::atan(name, eval(expr).await?),

        // --- integer ---
        Function::Div { dividend, divisor } => {
            let dividend = eval(dividend).await?;
            let divisor = eval(divisor).await?;

            f::div(name, dividend, divisor)
        }
        Function::Mod { dividend, divisor } => {
            let dividend = eval(dividend).await?;
            let divisor = eval(divisor).await?;

            dividend.modulo(&divisor)
        }
        Function::Gcd { left, right } => {
            let left = eval(left).await?;
            let right = eval(right).await?;

            f::gcd(name, left, right)
        }
        Function::Lcm { left, right } => {
            let left = eval(left).await?;
            let right = eval(right).await?;

            f::lcm(name, left, right)
        }

        // --- etc ---
        Function::Unwrap { expr, selector } => {
            let expr = eval(expr).await?;
            let selector = eval(selector).await?;

            f::unwrap(name, expr, selector)
        }
        Function::GenerateUuid() => Ok(f::generate_uuid()),
        Function::Now() => Ok(Evaluated::from(Value::Timestamp(Utc::now().naive_utc()))),
        Function::Format { expr, format } => {
            let expr = eval(expr).await?;
            let format = eval(format).await?;

            f::format(name, expr, format)
        }
        Function::ToDate { expr, format } => {
            let expr = eval(expr).await?;
            let format = eval(format).await?;
            f::to_date(name, expr, format)
        }
        Function::ToTimestamp { expr, format } => {
            let expr = eval(expr).await?;
            let format = eval(format).await?;
            f::to_timestamp(name, expr, format)
        }
        Function::ToTime { expr, format } => {
            let expr = eval(expr).await?;
            let format = eval(format).await?;
            f::to_time(name, expr, format)
        }
        Function::Position {
            from_expr,
            sub_expr,
        } => {
            let from_expr = eval(from_expr).await?;
            let sub_expr = eval(sub_expr).await?;
            f::position(from_expr, sub_expr)
        }
        Function::FindIdx {
            from_expr,
            sub_expr,
            start,
        } => {
            let from_expr = eval(from_expr).await?;
            let sub_expr = eval(sub_expr).await?;
            let start = match start {
                Some(idx) => Some(eval(idx).await?),
                None => None,
            };
            f::find_idx(name, from_expr, sub_expr, start)
        }
        Function::Cast { expr, data_type } => {
            let expr = eval(expr).await?;
            f::cast(expr, data_type)
        }
        Function::Extract { field, expr } => {
            let expr = eval(expr).await?;
            f::extract(field, expr)
        }

        // --- list ---
        Function::Append { expr, value } => {
            let expr = eval(expr).await?;
            let value = eval(value).await?;
            f::append(expr, value)
        }
    }
}
